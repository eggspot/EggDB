//! The target side of the XDND drag-and-drop protocol, version 5 (freedesktop.org, "Drag-and-Drop
//! Protocol for the X Window System"), from its public specification: the window announces
//! `XdndAware`, a source sends `XdndEnter`, `XdndPosition`, `XdndLeave` and `XdndDrop` as client
//! messages, the target answers each position with `XdndStatus` and a drop with `XdndFinished`,
//! and reads the dragged files from the `XdndSelection` selection as `text/uri-list` (RFC
//! 2483). [`XdndTarget`] is the state machine without I/O: it turns each message into the
//! [`XdndReaction`]s the window performs, so the protocol is tested without a server.

use std::path::PathBuf;

use super::WindowEvent;
use super::x11::CURRENT_TIME;

/// The XDND version this target speaks.
pub const XDND_VERSION: u32 = 5;
/// The oldest source version accepted: version 3 is the first whose messages carry what this
/// target reads.
pub const OLDEST_SOURCE_VERSION: u32 = 3;
/// The most paths one drop gives.
pub const MOST_DROPPED_PATHS: usize = 1024;

/// The atoms of the protocol, interned once by the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct XdndAtoms {
    /// `XdndAware`, the property naming the version on the target window.
    pub aware: u32,
    /// `XdndEnter`.
    pub enter: u32,
    /// `XdndPosition`.
    pub position: u32,
    /// `XdndStatus`.
    pub status: u32,
    /// `XdndLeave`.
    pub leave: u32,
    /// `XdndDrop`.
    pub drop: u32,
    /// `XdndFinished`.
    pub finished: u32,
    /// `XdndSelection`, the selection the dragged data is read from.
    pub selection: u32,
    /// `XdndTypeList`, the source's property listing more than three types.
    pub type_list: u32,
    /// `XdndActionCopy`.
    pub action_copy: u32,
    /// `text/uri-list`.
    pub uri_list: u32,
}

/// A client message of the protocol, sent by a drag source to this window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XdndMessage {
    /// A drag came over the window.
    Enter {
        /// The source window.
        source: u32,
        /// The protocol version the source speaks.
        version: u32,
        /// Whether the source lists more than three types in `XdndTypeList`.
        more_types: bool,
        /// The first three types (0 where there are fewer).
        types: [u32; 3],
    },
    /// The pointer moved over the window during the drag.
    Position {
        /// The source window.
        source: u32,
        /// The time to read the data with.
        time: u32,
    },
    /// The drag left the window, or was cancelled.
    Leave {
        /// The source window.
        source: u32,
    },
    /// The user dropped onto the window.
    Drop {
        /// The source window.
        source: u32,
        /// The time to read the data with.
        time: u32,
    },
}

/// The message a client message of type `kind` with `data` is, `None` for any other.
pub fn parse_message(atoms: &XdndAtoms, kind: u32, data: [u32; 5]) -> Option<XdndMessage> {
    let source = data[0];
    if kind == atoms.enter {
        Some(XdndMessage::Enter {
            source,
            version: data[1] >> 24,
            more_types: data[1] & 1 != 0,
            types: [data[2], data[3], data[4]],
        })
    } else if kind == atoms.position {
        Some(XdndMessage::Position {
            source,
            time: data[3],
        })
    } else if kind == atoms.leave {
        Some(XdndMessage::Leave { source })
    } else if kind == atoms.drop {
        Some(XdndMessage::Drop {
            source,
            time: data[2],
        })
    } else {
        None
    }
}

/// The values of an `XdndStatus` from `target`: whether it accepts the drop, an empty
/// rectangle (so the source sends every move) and the action it takes, `copy` or none.
pub fn status_data(atoms: &XdndAtoms, target: u32, accept: bool) -> [u32; 5] {
    let action = if accept { atoms.action_copy } else { 0 };
    [target, u32::from(accept), 0, 0, action]
}

/// The values of an `XdndFinished` from `target`: whether it took the drop and the action it
/// performed (version 5).
pub fn finished_data(atoms: &XdndAtoms, target: u32, accepted: bool) -> [u32; 5] {
    let action = if accepted { atoms.action_copy } else { 0 };
    [target, u32::from(accepted), action, 0, 0]
}

/// What the window does for a message.
#[derive(Debug, Clone, PartialEq)]
pub enum XdndReaction {
    /// Read the source's `XdndTypeList` property and call [`XdndTarget::types_read`].
    ReadTypeList {
        /// The source window.
        source: u32,
    },
    /// Send `XdndStatus` to the source.
    Status {
        /// The source window.
        source: u32,
        /// Whether the drop would be taken.
        accept: bool,
    },
    /// Ask for `XdndSelection` as `text/uri-list` at `time` and, when the answer comes,
    /// read it only if [`XdndTarget::awaits`] its time, then call [`XdndTarget::data`].
    RequestPaths {
        /// The time from the source's message.
        time: u32,
    },
    /// Send `XdndFinished` to the source.
    Finished {
        /// The source window.
        source: u32,
        /// Whether the drop was taken.
        accepted: bool,
    },
    /// Give the application this event.
    Event(WindowEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Drag {
    source: u32,
    offers_paths: bool,
    requested: bool,
    awaiting: Option<u32>,
    paths: Option<Vec<PathBuf>>,
    dropped: bool,
    entered: bool,
}

/// The drag over the window, from `XdndEnter` until `XdndLeave` or the end of a drop.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct XdndTarget {
    drag: Option<Drag>,
}

impl XdndTarget {
    /// Whether a drag is over the window.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Answers `message` (with `uri_list` the `text/uri-list` atom).
    pub fn receive(&mut self, message: XdndMessage, uri_list: u32) -> Vec<XdndReaction> {
        match message {
            XdndMessage::Enter {
                source,
                version,
                more_types,
                types,
            } => self.enter(source, version, more_types, &types, uri_list),
            XdndMessage::Position { source, time } => self.position(source, time),
            XdndMessage::Leave { source } => self.leave(source),
            XdndMessage::Drop { source, time } => self.drop(source, time),
        }
    }

    fn enter(
        &mut self,
        source: u32,
        version: u32,
        more_types: bool,
        types: &[u32],
        uri_list: u32,
    ) -> Vec<XdndReaction> {
        let mut reactions = self.leave_current();
        if version < OLDEST_SOURCE_VERSION {
            return reactions;
        }
        self.drag = Some(Drag {
            source,
            offers_paths: !more_types && types.contains(&uri_list),
            requested: false,
            awaiting: None,
            paths: None,
            dropped: false,
            entered: false,
        });
        if more_types {
            reactions.push(XdndReaction::ReadTypeList { source });
        }
        reactions
    }

    /// The source's `XdndTypeList` was read as `types`.
    pub fn types_read(&mut self, source: u32, types: &[u32], uri_list: u32) {
        if let Some(drag) = self.drag.as_mut().filter(|drag| drag.source == source) {
            drag.offers_paths = types.contains(&uri_list);
        }
    }

    fn position(&mut self, source: u32, time: u32) -> Vec<XdndReaction> {
        let Some(drag) = self.drag.as_mut().filter(|drag| drag.source == source) else {
            return vec![XdndReaction::Status {
                source,
                accept: false,
            }];
        };
        let accept = drag.offers_paths && drag.paths.as_ref().is_none_or(|paths| !paths.is_empty());
        let mut reactions = vec![XdndReaction::Status { source, accept }];
        if drag.offers_paths && !drag.requested {
            drag.requested = true;
            drag.awaiting = Some(time);
            reactions.push(XdndReaction::RequestPaths { time });
        }
        reactions
    }

    fn leave(&mut self, source: u32) -> Vec<XdndReaction> {
        if self.drag.as_ref().is_some_and(|drag| drag.source == source) {
            self.leave_current()
        } else {
            Vec::new()
        }
    }

    fn leave_current(&mut self) -> Vec<XdndReaction> {
        match self.drag.take() {
            Some(drag) if drag.entered => vec![XdndReaction::Event(WindowEvent::FileDragLeft)],
            _ => Vec::new(),
        }
    }

    fn drop(&mut self, source: u32, time: u32) -> Vec<XdndReaction> {
        let refused = XdndReaction::Finished {
            source,
            accepted: false,
        };
        let Some(drag) = self.drag.as_mut().filter(|drag| drag.source == source) else {
            return vec![refused];
        };
        if !drag.offers_paths {
            let mut reactions = self.leave_current();
            reactions.push(refused);
            return reactions;
        }
        drag.dropped = true;
        if drag.paths.is_some() {
            return self.finish();
        }
        if drag.requested {
            return Vec::new();
        }
        drag.requested = true;
        drag.awaiting = Some(time);
        vec![XdndReaction::RequestPaths { time }]
    }

    /// Whether an answer from `XdndSelection` stamped `time` is the one the current drag asked
    /// for: it asked and has had no answer yet, at that time (or the owner answered with
    /// `CurrentTime`). An answer left over from an earlier drag is not.
    pub fn awaits(&self, time: u32) -> bool {
        self.drag
            .as_ref()
            .and_then(|drag| drag.awaiting)
            .is_some_and(|asked| time == asked || time == CURRENT_TIME)
    }

    /// The `text/uri-list` the source gave at `time`, or `None` when it refused: before a drop
    /// the drag-over ring shows for its files, after one they are dropped. An answer the drag
    /// does not [`await`](Self::awaits) is ignored.
    pub fn data(&mut self, time: u32, uri_list: Option<&[u8]>) -> Vec<XdndReaction> {
        if !self.awaits(time) {
            return Vec::new();
        }
        let Some(drag) = self.drag.as_mut() else {
            return Vec::new();
        };
        drag.awaiting = None;
        let paths = uri_list.map(parse_uri_list).unwrap_or_default();
        if drag.dropped {
            drag.paths = Some(paths);
            return self.finish();
        }
        let shows = !paths.is_empty();
        drag.paths = Some(paths.clone());
        if !shows {
            return Vec::new();
        }
        drag.entered = true;
        vec![XdndReaction::Event(WindowEvent::FileDragEntered(paths))]
    }

    fn finish(&mut self) -> Vec<XdndReaction> {
        let Some(drag) = self.drag.take() else {
            return Vec::new();
        };
        let paths = drag.paths.unwrap_or_default();
        let accepted = !paths.is_empty();
        let event = if accepted {
            Some(WindowEvent::FilesDropped(paths))
        } else {
            drag.entered.then_some(WindowEvent::FileDragLeft)
        };
        let mut reactions: Vec<XdndReaction> = event.into_iter().map(XdndReaction::Event).collect();
        reactions.push(XdndReaction::Finished {
            source: drag.source,
            accepted,
        });
        reactions
    }
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        b'A'..=b'F' => Some(digit - b'A' + 10),
        _ => None,
    }
}

fn percent_decoded(text: &[u8]) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut at = 0;
    while let Some(&byte) = text.get(at) {
        if byte == b'%' {
            let high = hex_value(*text.get(at + 1)?)?;
            let low = hex_value(*text.get(at + 2)?)?;
            bytes.push((high << 4) | low);
            at += 3;
        } else {
            bytes.push(byte);
            at += 1;
        }
    }
    Some(bytes)
}

fn file_path(uri: &[u8]) -> Option<PathBuf> {
    let rest = uri.strip_prefix(b"file:")?;
    let path = match rest.strip_prefix(b"//") {
        Some(authority_and_path) => {
            let slash = authority_and_path.iter().position(|&byte| byte == b'/')?;
            let (host, path) = authority_and_path.split_at(slash);
            if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
                return None;
            }
            path
        }
        None => rest,
    };
    if !path.starts_with(b"/") {
        return None;
    }
    let decoded = percent_decoded(path)?;
    if decoded.contains(&0) {
        return None;
    }
    String::from_utf8(decoded).ok().map(PathBuf::from)
}

/// The local files of a `text/uri-list` (RFC 2483): one URI a line, lines ending in CRLF (or
/// LF), `#` lines comments; `file:` URIs on this host (an empty host or `localhost`),
/// percent-decoded. Other URIs, other hosts, and paths that are not UTF-8 or hold a NUL are
/// left out, and at most [`MOST_DROPPED_PATHS`] are given.
pub fn parse_uri_list(text: &[u8]) -> Vec<PathBuf> {
    text.split(|&byte| byte == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line).trim_ascii())
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(file_path)
        .take(MOST_DROPPED_PATHS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const URI_LIST: u32 = 50;

    fn atoms() -> XdndAtoms {
        XdndAtoms {
            aware: 40,
            enter: 41,
            position: 42,
            status: 43,
            leave: 44,
            drop: 45,
            finished: 46,
            selection: 47,
            type_list: 48,
            action_copy: 49,
            uri_list: URI_LIST,
        }
    }

    fn shop() -> Vec<PathBuf> {
        vec![PathBuf::from("/home/ana/shop.eggdb")]
    }

    #[test]
    fn messages_parse_from_their_five_values() {
        let atoms = atoms();
        assert_eq!(
            parse_message(&atoms, 41, [9, (5 << 24) | 1, URI_LIST, 0, 0]),
            Some(XdndMessage::Enter {
                source: 9,
                version: 5,
                more_types: true,
                types: [URI_LIST, 0, 0]
            })
        );
        assert_eq!(
            parse_message(&atoms, 42, [9, 0, (10 << 16) | 20, 777, 49]),
            Some(XdndMessage::Position {
                source: 9,
                time: 777
            })
        );
        assert_eq!(
            parse_message(&atoms, 44, [9, 0, 0, 0, 0]),
            Some(XdndMessage::Leave { source: 9 })
        );
        assert_eq!(
            parse_message(&atoms, 45, [9, 0, 888, 0, 0]),
            Some(XdndMessage::Drop {
                source: 9,
                time: 888
            })
        );
        assert_eq!(parse_message(&atoms, 99, [0; 5]), None);
        assert_eq!(status_data(&atoms, 3, true), [3, 1, 0, 0, 49]);
        assert_eq!(status_data(&atoms, 3, false), [3, 0, 0, 0, 0]);
        assert_eq!(finished_data(&atoms, 3, true), [3, 1, 49, 0, 0]);
    }

    #[test]
    fn a_file_drag_reads_the_paths_shows_the_ring_and_drops() {
        let mut target = XdndTarget::default();
        let enter = XdndMessage::Enter {
            source: 9,
            version: 5,
            more_types: false,
            types: [URI_LIST, 0, 0],
        };
        assert_eq!(target.receive(enter, URI_LIST), vec![]);
        assert_eq!(
            target.receive(XdndMessage::Position { source: 9, time: 5 }, URI_LIST),
            vec![
                XdndReaction::Status {
                    source: 9,
                    accept: true
                },
                XdndReaction::RequestPaths { time: 5 }
            ]
        );
        assert_eq!(
            target.data(5, Some(b"file:///home/ana/shop.eggdb\r\n")),
            vec![XdndReaction::Event(WindowEvent::FileDragEntered(shop()))]
        );
        assert_eq!(
            target.receive(XdndMessage::Position { source: 9, time: 6 }, URI_LIST),
            vec![XdndReaction::Status {
                source: 9,
                accept: true
            }]
        );
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 7 }, URI_LIST),
            vec![
                XdndReaction::Event(WindowEvent::FilesDropped(shop())),
                XdndReaction::Finished {
                    source: 9,
                    accepted: true
                }
            ]
        );
        assert!(!target.is_dragging());
    }

    #[test]
    fn a_drop_before_the_paths_arrive_waits_for_them() {
        let mut target = XdndTarget::default();
        target.receive(
            XdndMessage::Enter {
                source: 9,
                version: 5,
                more_types: true,
                types: [0; 3],
            },
            URI_LIST,
        );
        target.types_read(9, &[7, URI_LIST], URI_LIST);
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 7 }, URI_LIST),
            vec![XdndReaction::RequestPaths { time: 7 }]
        );
        assert_eq!(
            target.data(
                7,
                Some(b"# a comment\nfile://localhost/home/ana/shop.eggdb\n")
            ),
            vec![
                XdndReaction::Event(WindowEvent::FilesDropped(shop())),
                XdndReaction::Finished {
                    source: 9,
                    accepted: true
                }
            ]
        );
    }

    #[test]
    fn drags_without_files_are_refused_and_a_leave_ends_the_ring() {
        let mut target = XdndTarget::default();
        let text_only = XdndMessage::Enter {
            source: 9,
            version: 5,
            more_types: false,
            types: [7, 0, 0],
        };
        target.receive(text_only, URI_LIST);
        assert_eq!(
            target.receive(XdndMessage::Position { source: 9, time: 1 }, URI_LIST),
            vec![XdndReaction::Status {
                source: 9,
                accept: false
            }]
        );
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 2 }, URI_LIST),
            vec![XdndReaction::Finished {
                source: 9,
                accepted: false
            }]
        );
        let files = XdndMessage::Enter {
            source: 9,
            version: 5,
            more_types: false,
            types: [URI_LIST, 0, 0],
        };
        target.receive(files, URI_LIST);
        target.receive(XdndMessage::Position { source: 9, time: 3 }, URI_LIST);
        target.data(3, Some(b"file:///home/ana/shop.eggdb"));
        assert_eq!(
            target.receive(XdndMessage::Leave { source: 8 }, URI_LIST),
            vec![]
        );
        assert_eq!(
            target.receive(XdndMessage::Leave { source: 9 }, URI_LIST),
            vec![XdndReaction::Event(WindowEvent::FileDragLeft)]
        );
        let old = XdndMessage::Enter {
            source: 9,
            version: 2,
            more_types: false,
            types: [URI_LIST, 0, 0],
        };
        target.receive(old, URI_LIST);
        assert!(!target.is_dragging());
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 2 }, URI_LIST),
            vec![XdndReaction::Finished {
                source: 9,
                accepted: false
            }]
        );
    }

    #[test]
    fn a_refused_or_empty_uri_list_drops_nothing() {
        let mut target = XdndTarget::default();
        let files = XdndMessage::Enter {
            source: 9,
            version: 5,
            more_types: false,
            types: [URI_LIST, 0, 0],
        };
        target.receive(files, URI_LIST);
        target.receive(XdndMessage::Position { source: 9, time: 3 }, URI_LIST);
        assert_eq!(target.data(3, None), vec![]);
        assert_eq!(
            target.receive(XdndMessage::Position { source: 9, time: 4 }, URI_LIST),
            vec![XdndReaction::Status {
                source: 9,
                accept: false
            }]
        );
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 5 }, URI_LIST),
            vec![XdndReaction::Finished {
                source: 9,
                accepted: false
            }]
        );
    }

    #[test]
    fn an_answer_left_over_from_an_earlier_drag_is_ignored() {
        let mut target = XdndTarget::default();
        let files = |source| XdndMessage::Enter {
            source,
            version: 5,
            more_types: false,
            types: [URI_LIST, 0, 0],
        };
        target.receive(files(8), URI_LIST);
        target.receive(XdndMessage::Position { source: 8, time: 3 }, URI_LIST);
        target.receive(XdndMessage::Leave { source: 8 }, URI_LIST);
        target.receive(files(9), URI_LIST);
        assert!(!target.awaits(3));
        assert_eq!(
            target.data(3, Some(b"file:///home/ana/other.eggdb")),
            vec![]
        );
        target.receive(XdndMessage::Position { source: 9, time: 6 }, URI_LIST);
        assert!(!target.awaits(3));
        assert!(target.awaits(6));
        assert_eq!(
            target.data(3, Some(b"file:///home/ana/other.eggdb")),
            vec![]
        );
        assert_eq!(
            target.data(6, Some(b"file:///home/ana/shop.eggdb")),
            vec![XdndReaction::Event(WindowEvent::FileDragEntered(shop()))]
        );
        assert!(!target.awaits(6));
        assert_eq!(
            target.data(6, Some(b"file:///home/ana/other.eggdb")),
            vec![]
        );
        assert_eq!(
            target.receive(XdndMessage::Drop { source: 9, time: 7 }, URI_LIST),
            vec![
                XdndReaction::Event(WindowEvent::FilesDropped(shop())),
                XdndReaction::Finished {
                    source: 9,
                    accepted: true
                }
            ]
        );
    }

    #[test]
    fn an_owner_answering_with_current_time_is_taken() {
        let mut target = XdndTarget::default();
        target.receive(
            XdndMessage::Enter {
                source: 9,
                version: 5,
                more_types: false,
                types: [URI_LIST, 0, 0],
            },
            URI_LIST,
        );
        target.receive(XdndMessage::Drop { source: 9, time: 7 }, URI_LIST);
        assert!(target.awaits(CURRENT_TIME));
        assert_eq!(
            target.data(CURRENT_TIME, Some(b"file:///home/ana/shop.eggdb")),
            vec![
                XdndReaction::Event(WindowEvent::FilesDropped(shop())),
                XdndReaction::Finished {
                    source: 9,
                    accepted: true
                }
            ]
        );
    }

    #[test]
    fn uri_lists_give_local_files_only() {
        assert_eq!(
            parse_uri_list(
                b"file:///tmp/a%20b.eggdb\r\n#comment\r\nhttp://example.com/x\r\nfile://other/tmp/c\r\nfile:/tmp/d\r\n\r\nfile:///tmp/caf%C3%A9\r\n"
            ),
            vec![
                PathBuf::from("/tmp/a b.eggdb"),
                PathBuf::from("/tmp/d"),
                PathBuf::from("/tmp/caf\u{e9}"),
            ]
        );
        assert_eq!(parse_uri_list(b"file:///tmp/bad%2"), Vec::<PathBuf>::new());
        assert_eq!(parse_uri_list(b"file:///tmp/nul%00"), Vec::<PathBuf>::new());
        assert_eq!(parse_uri_list(b"file:///tmp/%FF"), Vec::<PathBuf>::new());
        assert_eq!(parse_uri_list(b"file:relative"), Vec::<PathBuf>::new());
        assert_eq!(parse_uri_list(b"file://"), Vec::<PathBuf>::new());
        let many = "file:///x\n".repeat(MOST_DROPPED_PATHS + 5);
        assert_eq!(parse_uri_list(many.as_bytes()).len(), MOST_DROPPED_PATHS);
    }
}
