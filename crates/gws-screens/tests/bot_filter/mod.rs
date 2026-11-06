//! A screen as the Bot's `ui_look` and `ui_act` see it. What's copied below keeps
//! its names and its order, so it can be read against the original.
//!
//! The rule the tests hold: nothing the AI can stage may sit behind a person's
//! press. The AI can only reach a control the filter names, so a control that
//! changes what a press would send has to go unnamed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nsh_core::{MountItem, NodeId};
use nsh_testing::Headless;
use nsh_testing::agent::{Model, Scene as _, SemanticNode};

/// A node as `nsh_platform_macos::ui_tree` gives it to the Bot's filter.
#[derive(Debug, Clone)]
pub struct Seen {
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub enabled: bool,
    pub states: Vec<String>,
    pub children: Vec<Seen>,
}

// --- Copied from the Bot: buwiz-bot `app/src/agent_hub/filter.rs` --------------
// `FIELDS`, `OTHER_CONTROLS`, `TEXT`, `LABEL_MAX`, `DIGITS_MAX`, `is_control`,
// `plain`, `words`, `grouped`, `inside`, `holds_note`, `button_label`,
// `field_name`, `pressable` and `typable`, unchanged. If the Bot's change, these
// have to change with them: nothing here can notice.

const FIELDS: [&str; 8] = [
    "textfield",
    "search",
    "combobox",
    "checkbox",
    "switch",
    "radio",
    "slider",
    "spinbutton",
];
const OTHER_CONTROLS: [&str; 5] = ["link", "tab", "option", "menuitem", "treeitem"];
const TEXT: [&str; 5] = ["text", "note", "heading", "status", "alert"];

const LABEL_MAX: usize = 48;
const DIGITS_MAX: usize = 6;

fn is_control(role: &str) -> bool {
    role == "button" || FIELDS.contains(&role) || OTHER_CONTROLS.contains(&role)
}

fn plain(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= LABEL_MAX
        && s.chars().filter(char::is_ascii_digit).count() <= DIGITS_MAX
        && !s.contains('@')
        && !s.chars().any(char::is_control)
}

fn words(n: &Seen) -> &str {
    n.value
        .as_deref()
        .filter(|v| !v.is_empty())
        .unwrap_or(&n.name)
        .trim()
}

fn grouped(n: &Seen) -> bool {
    n.states.iter().any(|s| s == "grouped")
}

fn inside<'a>(n: &'a Seen, texts: &mut Vec<&'a str>) -> bool {
    n.children.iter().fold(false, |control, c| {
        if TEXT.contains(&c.role.as_str()) {
            texts.push(words(c));
        }
        inside(c, texts) || is_control(&c.role) || control
    })
}

fn holds_note(n: &Seen) -> bool {
    n.children.iter().any(|c| c.role == "note" || holds_note(c))
}

fn button_label(n: &Seen) -> Option<&str> {
    let mut texts = Vec::new();
    if inside(n, &mut texts) || grouped(n) {
        return None;
    }
    let name = n.name.trim();
    let label = match texts[..] {
        [] => name,
        [text] if name == text || (name.is_empty() && !holds_note(n)) => text,
        _ => return None,
    };
    plain(label).then_some(label)
}

fn field_name(n: &Seen) -> Option<&str> {
    Some(n.name.trim()).filter(|name| !grouped(n) && plain(name))
}

fn pressable(role: &str) -> bool {
    ["button", "checkbox", "switch", "radio"].contains(&role)
}

fn typable(role: &str) -> bool {
    ["textfield", "search", "combobox", "spinbutton"].contains(&role)
}

// --- End of what's copied ------------------------------------------------------

/// `node`'s role as the Mac's `access::spec_for` has it. Headless gives its own
/// names, and "generic" to a labelled box the Mac calls a group. A box with no
/// label is no node there: its children stand in its place.
fn mac_role(node: &SemanticNode) -> Option<&str> {
    Some(match node.role.as_str() {
        "generic" | "scroll" if node.label.is_none() => return None,
        "generic" | "scroll" => "group",
        "textbox" => "textfield",
        "imagebutton" | "keyboardkey" => "button",
        "adjustable" => "slider",
        "header" => "heading",
        "summary" => "status",
        "progressbar" => "progress",
        other => other,
    })
}

fn seen_from(nodes: &[SemanticNode], parent: Option<NodeId>) -> Vec<Seen> {
    let mut out = Vec::new();
    for node in nodes.iter().filter(|n| n.parent == parent) {
        let below = seen_from(nodes, Some(node.id));
        let Some(role) = mac_role(node) else {
            out.extend(below);
            continue;
        };
        let label = node.label.clone().unwrap_or_default();
        // the Mac reads a text's words as its value, and gives it no name
        let reads = ["text", "heading", "option", "treeitem"].contains(&role);
        let (name, value) = match (reads, &node.value) {
            (true, Some(value)) => (String::new(), Some(format!("{label}, {value}"))),
            (true, None) => (String::new(), Some(label)),
            (false, value) => (label, value.clone()),
        };
        let group = node.grouped && role != "text";
        let mut states = node.states.clone();
        if group {
            states.push("grouped".to_owned());
        }
        out.push(Seen {
            role: role.to_owned(),
            name,
            value,
            enabled: node.enabled,
            states,
            children: if group { Vec::new() } else { below },
        });
    }
    out
}

fn walk<'a>(nodes: &'a [Seen], out: &mut Vec<&'a Seen>) {
    for n in nodes {
        out.push(n);
        walk(&n.children, out);
    }
}

/// A view's parent, its left edge there and its width, as the engine gave them.
type Placed = (Option<NodeId>, f32, f32);

/// A headless app's screen, read as the Bot reads a real one, and where the
/// engine really put its views, as `shows` can't say.
#[derive(Clone)]
pub struct Filter {
    model: Arc<Model>,
    views: Arc<Mutex<HashMap<NodeId, Placed>>>,
    texts: Arc<Mutex<HashMap<NodeId, String>>>,
}

impl Filter {
    /// From now on. Do this before the viewport is set, so no view is missed;
    /// it takes the app's one observer.
    pub fn watch(app: &mut Headless) -> Self {
        let filter = Self {
            model: Arc::new(Model::default()),
            views: Arc::default(),
            texts: Arc::default(),
        };
        let fed = filter.clone();
        app.observe(move |_, batch| {
            fed.model.apply(batch);
            let mut views = fed.views.lock().unwrap();
            let mut texts = fed.texts.lock().unwrap();
            for item in &batch.items {
                match item {
                    MountItem::Insert { parent, child, .. } => {
                        views.entry(*child).or_default().0 = Some(*parent);
                    }
                    MountItem::Frame { id, frame } => {
                        let view = views.entry(*id).or_default();
                        (view.1, view.2) = (frame.origin.x.0, frame.size.width.0);
                    }
                    MountItem::Text { id, text, .. } => {
                        texts.insert(*id, text.text.clone());
                    }
                    _ => {}
                }
            }
        });
        filter
    }

    pub fn tree(&self) -> Vec<Seen> {
        seen_from(&self.model.semantic_tree().expect("a tree"), None)
    }

    /// The text reading `line` lies whole inside `width`. Drive's tests have
    /// frames of their own, so they don't call this.
    #[allow(dead_code)]
    pub fn assert_inside(&self, line: &str, width: f32) {
        let views = self.views.lock().unwrap();
        let id = {
            let texts = self.texts.lock().unwrap();
            let mut found = texts.iter().filter(|(_, t)| t.as_str() == line);
            *found.next().unwrap_or_else(|| panic!("no text {line:?}")).0
        };
        let (mut at, mut left) = (id, 0.0);
        while let Some((parent, x, _)) = views.get(&at) {
            left += x;
            let Some(parent) = parent else { break };
            at = *parent;
        }
        let right = left + views[&id].2;
        assert!(
            left >= -0.5 && right <= width + 0.5,
            "{line:?} at {width} runs from {left} to {right}"
        );
    }

    /// Every control the AI can act on: `(role, what it's named)`.
    pub fn reachable(&self) -> Vec<(String, String)> {
        let tree = self.tree();
        let mut all = Vec::new();
        walk(&tree, &mut all);
        all.iter()
            .filter(|n| n.enabled)
            .filter_map(|n| {
                let role = n.role.as_str();
                let name = if role == "button" {
                    button_label(n)
                } else if FIELDS.contains(&role) {
                    field_name(n)
                } else {
                    None
                };
                let acts = pressable(role) || typable(role);
                name.filter(|_| acts)
                    .map(|name| (role.to_owned(), name.to_owned()))
            })
            .collect()
    }

    /// How many fields are on screen with no name, so out of the AI's reach.
    pub fn unnamed_fields(&self) -> usize {
        let tree = self.tree();
        let mut all = Vec::new();
        walk(&tree, &mut all);
        let fields = all.iter().filter(|n| FIELDS.contains(&n.role.as_str()));
        fields.filter(|n| field_name(n).is_none()).count()
    }

    /// Every accessibility label on screen: a text has none, only words.
    pub fn labels(&self) -> Vec<String> {
        let tree = self.tree();
        let mut all = Vec::new();
        walk(&tree, &mut all);
        all.iter()
            .filter(|n| !n.name.is_empty())
            .map(|n| n.name.clone())
            .collect()
    }

    /// No label holds any of `secrets`, or an address but the fixed sample.
    pub fn assert_no_data_in_labels(&self, secrets: &[&str], at: &str) {
        for label in self.labels() {
            let low = label.to_lowercase();
            for secret in secrets {
                assert!(
                    !low.contains(&secret.to_lowercase()),
                    "{at}: the label {label:?} holds {secret}"
                );
            }
            assert!(
                !label.contains('@') || label.contains("name@company.com"),
                "{at}: the label {label:?} holds an address"
            );
        }
    }

    /// The AI reaches only what's in `may`: controls that stage nothing.
    pub fn assert_stages_nothing(&self, may: &[&str], at: &str) {
        for (role, name) in self.reachable() {
            assert!(
                may.contains(&name.as_str()),
                "{at}: the AI can reach the {role} {name:?}, and nobody has said why that's safe"
            );
        }
    }
}

/// The copy above has to work, or an empty list of what's reachable proves
/// nothing. These are the Bot's own cases.
pub fn assert_the_copy_works() {
    let text = |s: &str| Seen {
        role: "text".to_owned(),
        name: String::new(),
        value: Some(s.to_owned()),
        enabled: true,
        states: Vec::new(),
        children: Vec::new(),
    };
    let button = |name: &str, children: Vec<Seen>| Seen {
        role: "button".to_owned(),
        name: name.to_owned(),
        value: None,
        enabled: true,
        states: Vec::new(),
        children,
    };
    assert_eq!(
        button_label(&button("Save", vec![text("Save")])),
        Some("Save")
    );
    assert_eq!(button_label(&button("Clear", vec![])), Some("Clear"));
    assert_eq!(button_label(&button("Open Juan", vec![text("Juan")])), None);
    let two = button("", vec![text("Sales"), text("3 branches")]);
    assert_eq!(button_label(&two), None);
    for s in [
        "000-111-222",
        "maria@example.test",
        "A name far too long to be a label on any button at all",
    ] {
        assert_eq!(button_label(&button(s, vec![])), None, "{s}");
    }
    assert_eq!(button_label(&button("Add 2026", vec![])), Some("Add 2026"));
}
