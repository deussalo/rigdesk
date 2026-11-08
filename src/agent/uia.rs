//! A labelled snapshot of the on-screen UI through UI Automation: the control tree with each element's role, name,
//! automation id and clickable centre, so the agent can read what is on screen and aim its pointer. Bounded in depth
//! and count so a deep tree cannot stall or flood the response.

use uiautomation::{UIAutomation, UIElement, UITreeWalker};

const MAX_NODES: usize = 400;
const MAX_DEPTH: usize = 12;

/// The control tree under the desktop root, as indented lines the agent reads. Each actionable element shows its
/// centre as `@x,y`, which is what the pointer tools take.
pub fn snapshot() -> Result<String, String> {
    let automation = UIAutomation::new().map_err(|e| format!("UI Automation unavailable: {e}"))?;
    let walker = automation.create_tree_walker().map_err(|e| format!("tree walker unavailable: {e}"))?;
    let root = automation.get_root_element().map_err(|e| format!("no root element: {e}"))?;
    let mut out = String::new();
    let mut count = 0usize;
    walk(&walker, &root, 0, &mut count, &mut out);
    if count >= MAX_NODES {
        out.push_str(&format!("... [snapshot capped at {MAX_NODES} elements]\n"));
    }
    Ok(out)
}

fn walk(walker: &UITreeWalker, element: &UIElement, depth: usize, count: &mut usize, out: &mut String) {
    if *count >= MAX_NODES || depth > MAX_DEPTH {
        return;
    }
    *count += 1;
    out.push_str(&line(element, depth));
    if let Ok(child) = walker.get_first_child(element) {
        walk(walker, &child, depth + 1, count, out);
        let mut sibling = child;
        while let Ok(next) = walker.get_next_sibling(&sibling) {
            if *count >= MAX_NODES {
                break;
            }
            walk(walker, &next, depth + 1, count, out);
            sibling = next;
        }
    }
}

fn line(element: &UIElement, depth: usize) -> String {
    let indent = "  ".repeat(depth);
    let role = element.get_control_type().map(|c| format!("{c:?}")).unwrap_or_else(|_| "?".into());
    let name = element.get_name().unwrap_or_default();
    let id = element.get_automation_id().unwrap_or_default();
    let named = if name.is_empty() { String::new() } else { format!(" {name:?}") };
    let ided = if id.is_empty() { String::new() } else { format!(" #{id}") };
    let at = match element.get_bounding_rectangle() {
        Ok(r) if r.get_right() > r.get_left() && r.get_bottom() > r.get_top() => {
            format!(" @{},{}", (r.get_left() + r.get_right()) / 2, (r.get_top() + r.get_bottom()) / 2)
        }
        _ => String::new(),
    };
    format!("{indent}{role}{named}{ided}{at}\n")
}
