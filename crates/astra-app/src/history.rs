//! Text snapshots preserve comments, formatting, and unknown fields across undo.
pub fn remember(
    history: &mut Vec<String>,
    future: &mut Vec<String>,
    previous: String,
    limit: usize,
) {
    history.push(previous);
    if history.len() > limit {
        history.drain(..history.len() - limit);
    }
    future.clear();
}
pub fn restore(
    history: &mut Vec<String>,
    future: &mut Vec<String>,
    current: String,
    redo: bool,
) -> Option<String> {
    let (from, to) = if redo {
        (future, history)
    } else {
        (history, future)
    };
    let text = from.pop()?;
    to.push(current);
    Some(text)
}
