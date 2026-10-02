use consortium_cli::tree::{render, NodeStatus, OutputFormat, TreeNode};

struct N {
    label: String,
    status: Option<NodeStatus>,
    meta: Vec<(String, String)>,
    kids: Vec<N>,
}
impl N {
    fn leaf(s: &str) -> Self {
        Self {
            label: s.into(),
            status: None,
            meta: vec![],
            kids: vec![],
        }
    }
    fn st(mut self, s: NodeStatus) -> Self {
        self.status = Some(s);
        self
    }
    fn m(mut self, k: &str, v: &str) -> Self {
        self.meta.push((k.into(), v.into()));
        self
    }
    fn k(mut self, ks: Vec<N>) -> Self {
        self.kids = ks;
        self
    }
}
impl TreeNode for N {
    fn label(&self) -> String {
        self.label.clone()
    }
    fn status(&self) -> Option<NodeStatus> {
        self.status.clone()
    }
    fn metadata(&self) -> Vec<(String, String)> {
        self.meta.clone()
    }
    fn children(&self) -> Vec<&dyn TreeNode> {
        self.kids.iter().map(|c| c as &dyn TreeNode).collect()
    }
}

fn main() {
    let cascade = N::leaf("seir (build host)")
        .st(NodeStatus::Ok)
        .m("round", "0")
        .k(vec![
            N::leaf("hp01")
                .st(NodeStatus::Ok)
                .m("round", "1")
                .m("dur", "12ms")
                .k(vec![
                    N::leaf("mm01")
                        .st(NodeStatus::Ok)
                        .m("round", "2")
                        .k(vec![N::leaf("mm05").st(NodeStatus::Ok).m("round", "3")]),
                    N::leaf("mm02").st(NodeStatus::InProgress).m("round", "2"),
                ]),
            N::leaf("hp02")
                .st(NodeStatus::Ok)
                .m("round", "1")
                .m("dur", "18ms")
                .k(vec![
                    N::leaf("hp03")
                        .st(NodeStatus::Failed)
                        .m("round", "2")
                        .m("err", "disk-full"),
                    N::leaf("contra").st(NodeStatus::Pending),
                ]),
        ]);

    println!("\n=== tree (color, no depth limit) ===\n");
    print!(
        "{}",
        render(
            &cascade,
            &OutputFormat::Tree {
                max_depth: None,
                color: true
            }
        )
    );

    println!("\n=== tree -L 2 (colorless) ===\n");
    print!(
        "{}",
        render(
            &cascade,
            &OutputFormat::Tree {
                max_depth: Some(2),
                color: false
            }
        )
    );

    println!("\n=== json ===\n");
    print!("{}", render(&cascade, &OutputFormat::Json));

    println!("\n\n=== yaml ===\n");
    print!("{}", render(&cascade, &OutputFormat::Yaml));

    println!("\n=== toml ===\n");
    print!("{}", render(&cascade, &OutputFormat::Toml));
}
