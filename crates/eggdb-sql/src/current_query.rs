//! The text the client sent for the request running now, as PostgreSQL's `current_query()`
//! answers it (D1307): statements that functions and procedures run inside it keep the
//! client's text.

use std::cell::RefCell;

const LARGEST_KEPT_BUFFER: usize = 64 * 1024;

struct ClientText {
    depth: usize,
    text: String,
}

thread_local! {
    static CLIENT_TEXT: RefCell<ClientText> = const {
        RefCell::new(ClientText {
            depth: 0,
            text: String::new(),
        })
    };
}

pub(crate) struct Entered;

impl Drop for Entered {
    fn drop(&mut self) {
        CLIENT_TEXT.with(|client| {
            let mut client = client.borrow_mut();
            client.depth = client.depth.saturating_sub(1);
            if client.depth == 0 && client.text.capacity() > LARGEST_KEPT_BUFFER {
                client.text = String::new();
            }
        });
    }
}

pub(crate) fn enter(text: &str) -> Entered {
    CLIENT_TEXT.with(|client| {
        let mut client = client.borrow_mut();
        if client.depth == 0 {
            client.text.clear();
            client.text.push_str(text);
        }
        client.depth += 1;
    });
    Entered
}

pub(crate) fn current() -> Option<String> {
    CLIENT_TEXT.with(|client| {
        let client = client.borrow();
        (client.depth > 0).then(|| client.text.clone())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_outermost_text_is_kept_while_inner_statements_run() {
        assert_eq!(current(), None);
        {
            let _outer = enter("SELECT f()");
            {
                let _inner = enter("SELECT current_query()");
                assert_eq!(current().as_deref(), Some("SELECT f()"));
            }
            assert_eq!(current().as_deref(), Some("SELECT f()"));
        }
        assert_eq!(current(), None);
    }
}
