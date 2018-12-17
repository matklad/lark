use crate::full_inference::analysis::Analysis;
use crate::full_inference::analysis::Node;
use crate::full_inference::analysis::NodeData;
use crate::full_inference::analysis::Path;
use crate::full_inference::analysis::PathData;
use crate::full_inference::FullInference;
use crate::full_inference::FullInferenceTables;
use crate::full_inference::Perm;
use crate::results::TypeCheckResults;
use lark_collections::map::Entry;
use lark_collections::FxIndexMap;
use lark_hir as hir;
use lark_ty as ty;
use lark_unify::UnificationTable;

crate struct AnalysisBuilder<'me> {
    analysis: Analysis,
    fn_body: &'me hir::FnBody,
    results: &'me TypeCheckResults<FullInference>,
    unify: &'me mut UnificationTable<FullInferenceTables, hir::MetaIndex>,
    path_datas: FxIndexMap<PathData, ()>,
}

impl AnalysisBuilder<'_> {
    fn push_node(&mut self, data: NodeData) -> Node {
        self.analysis.node_datas.push(data)
    }

    fn push_node_edge(&mut self, start_node: Node, data: NodeData) -> Node {
        let n = self.push_node(data);
        self.push_edge(start_node, n);
        n
    }

    fn push_edge(&mut self, from: Node, to: Node) {
        self.analysis.cfg_edges.push((from, to));
    }

    fn node(&mut self, start_node: Node, n: impl IntoNode) -> Node {
        n.to_cfg_node(start_node, self)
    }

    /// Converts a HIR "Place" into an analysis *path*
    fn path(&mut self, place: hir::Place) -> Path {
        match self.fn_body[place] {
            hir::PlaceData::Variable(v) => self.create_path(PathData::Variable(v)),
            hir::PlaceData::Entity(e) => self.create_path(PathData::Entity(e)),
            hir::PlaceData::Temporary(e) => self.create_path(PathData::Temporary(e)),
            hir::PlaceData::Field { owner, name } => {
                let name = self.fn_body[name].text;
                let owner = self.path(owner);
                if false {
                    // dummy code to stop errors
                    self.create_path(PathData::Index { owner });
                }
                self.create_path(PathData::Field { owner, name })
            }
        }
    }

    fn create_path(&mut self, path_data: PathData) -> Path {
        match self.path_datas.entry(path_data) {
            Entry::Occupied(entry) => Path::from(entry.index()),
            Entry::Vacant(entry) => {
                let path = self.analysis.path_datas.push(path_data);
                assert_eq!(entry.index(), path.as_usize());
                entry.insert(());

                if let Some(owner) = path_data.owner() {
                    self.analysis.owner_paths.push((owner, path));
                }

                path
            }
        }
    }

    fn access(&mut self, perm: Perm, path: Path, node: Node) {
        self.analysis.accesses.push((perm, path, node));
    }

    fn generate_assignment_facts(&mut self, path: Path, node: Node) {
        let path_data = self.analysis.path_datas[path];

        if path_data.precise(&self.analysis.path_datas) {
            self.analysis.overwritten.push((path, node));
        }

        if let Some(owner) = path_data.owner() {
            self.analysis.traverse.push((owner, node));
        }
    }

    fn use_result_of(&mut self, node: Node, expression: hir::Expression) {
        let expression_ty = self.results.ty(expression);
        self.use_ty(node, expression_ty);
    }

    fn use_ty(&mut self, node: Node, ty: ty::Ty<FullInference>) {
        let ty::Ty {
            repr: ty::Erased,
            perm,
            base,
        } = ty;

        self.analysis.used.push((perm, node));

        match self.unify.shallow_resolve_data(base) {
            Ok(ty::BaseData { kind: _, generics }) => {
                for generic in generics.iter() {
                    match generic {
                        ty::GenericKind::Ty(t) => self.use_ty(node, t),
                    }
                }
            }

            Err(_) => {
                // All things should be inferrable. This will wind up
                // with an error getting reported later at the
                // conclusion of full-inference, so do nothing.
                //
                // (NB: It'd be nice to have a way to *assert* that,
                // as we do in rustc!)
            }
        }
    }
}

trait IntoNode: Copy {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node;
}

impl IntoNode for hir::Expression {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        match &builder.fn_body[self] {
            hir::ExpressionData::Let {
                initializer, body, ..
            } => {
                // First, we evaluate `I`...
                let initializer_node = builder.node(start_node, initializer);

                // Next, the result of that is assigned into the
                // variable `X`. This occurs at the node associated with the `let` itself.
                let self_node =
                    builder.push_node_edge(initializer_node, NodeData::Expression(self));
                if let Some(initializer) = initializer {
                    builder.use_result_of(self_node, *initializer);
                }

                // Finally, the body `B` is evaluated.
                builder.node(self_node, body)
            }

            hir::ExpressionData::Place { place, .. } => {
                let place_node = builder.node(start_node, place);
                let self_node = builder.push_node_edge(place_node, NodeData::Expression(self));

                let perm = builder.results.access_permissions[&self];
                let path = builder.path(*place);
                builder.access(perm, path, self_node);

                self_node
            }

            hir::ExpressionData::Assignment { place, value } => {
                let place_node = builder.node(start_node, place);
                let value_node = builder.node(place_node, value);
                let self_node = builder.push_node_edge(value_node, NodeData::Expression(self));

                let path = builder.path(*place);
                builder.generate_assignment_facts(path, self_node);

                self_node
            }

            hir::ExpressionData::MethodCall { arguments, .. } => {
                let arguments_node = builder.node(start_node, arguments);
                let self_node = builder.push_node_edge(arguments_node, NodeData::Expression(self));

                for argument in arguments.iter(builder.fn_body) {
                    builder.use_result_of(self_node, argument);
                }

                self_node
            }

            hir::ExpressionData::Call {
                function,
                arguments,
            } => {
                let function_node = builder.node(start_node, function);
                let arguments_node = builder.node(function_node, arguments);
                let self_node = builder.push_node_edge(arguments_node, NodeData::Expression(self));

                for argument in arguments.iter(builder.fn_body) {
                    builder.use_result_of(self_node, argument);
                }

                self_node
            }

            hir::ExpressionData::If {
                condition,
                if_true,
                if_false,
            } => {
                let condition_node = builder.node(start_node, condition);

                // We say that an `if` "executes" when the condition is tested:
                let self_node = builder.push_node_edge(condition_node, NodeData::Expression(self));
                builder.use_result_of(self_node, *condition);

                // Then the arms come afterwards:
                let if_true_node = builder.node(self_node, if_true);
                let if_false_node = builder.node(self_node, if_false);

                // Create a node to rejoin the control-flows:
                let join_node = builder.push_node(NodeData::Join(self));
                builder.push_edge(if_true_node, join_node);
                builder.push_edge(if_false_node, join_node);

                join_node
            }

            hir::ExpressionData::Binary { left, right, .. } => {
                let left_node = builder.node(start_node, left);
                let right_node = builder.node(left_node, right);
                let self_node = builder.push_node_edge(right_node, NodeData::Expression(self));
                builder.use_result_of(self_node, *left);
                builder.use_result_of(self_node, *right);
                self_node
            }

            hir::ExpressionData::Unary { value, .. } => {
                let value_node = builder.node(start_node, value);
                let self_node = builder.push_node_edge(value_node, NodeData::Expression(self));
                builder.use_result_of(self_node, *value);
                self_node
            }

            hir::ExpressionData::Error { .. }
            | hir::ExpressionData::Unit {}
            | hir::ExpressionData::Literal { .. } => {
                builder.push_node_edge(start_node, NodeData::Expression(self))
            }

            hir::ExpressionData::Aggregate { fields, .. } => {
                let field_node = builder.node(start_node, fields);
                let self_node = builder.push_node_edge(field_node, NodeData::Expression(self));
                for field in fields.iter(builder.fn_body) {
                    builder.use_result_of(self_node, builder.fn_body[field].expression);
                }
                self_node
            }

            hir::ExpressionData::Sequence { first, second } => {
                let first_node = builder.node(start_node, first);
                let second_node = builder.node(first_node, second);
                builder.push_node_edge(second_node, NodeData::Expression(self))
            }
        }
    }
}

impl IntoNode for hir::IdentifiedExpression {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        builder.node(start_node, builder.fn_body[self].expression)
    }
}

impl IntoNode for hir::Place {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        match &builder.fn_body[self] {
            hir::PlaceData::Variable(_) => start_node,

            hir::PlaceData::Entity(_) => start_node,

            hir::PlaceData::Temporary(expression) => builder.node(start_node, expression),

            hir::PlaceData::Field { owner, .. } => builder.node(start_node, owner),
        }
    }
}

impl<N: IntoNode + hir::HirIndex> IntoNode for hir::List<N> {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        let mut n = start_node;
        for elem in self.iter(builder.fn_body) {
            n = builder.node(n, elem);
        }
        n
    }
}

impl<N: IntoNode> IntoNode for Option<N> {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        match self {
            None => start_node,
            Some(v) => builder.node(start_node, v),
        }
    }
}

impl<N: IntoNode + Copy> IntoNode for &N {
    fn to_cfg_node(self, start_node: Node, builder: &mut AnalysisBuilder<'_>) -> Node {
        N::to_cfg_node(*self, start_node, builder)
    }
}
