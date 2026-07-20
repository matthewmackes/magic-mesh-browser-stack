//! Protocol guard for the Chromium helper lane: it must speak the same
//! BOOKMARKS-6 wire bytes as Servo and the shell client.

use mde_web_cef::wire::{ControlMsg, EventMsg};

#[test]
fn control_and_event_wire_bytes_are_pinned() {
    assert_eq!(
        ControlMsg::Load("https://example.com/".to_owned()).encode(),
        [
            0, 20, 0, 0, 0, b'h', b't', b't', b'p', b's', b':', b'/', b'/', b'e', b'x', b'a', b'm',
            b'p', b'l', b'e', b'.', b'c', b'o', b'm', b'/'
        ]
    );
    assert_eq!(ControlMsg::Stop.encode(), [8]);
    assert_eq!(
        ControlMsg::FillLogin {
            expected_host: "h".to_owned(),
            username: "u".to_owned(),
            password: "p".to_owned(),
        }
        .encode(),
        [35, 1, 0, 0, 0, b'h', 1, 0, 0, 0, b'u', 1, 0, 0, 0, b'p']
    );
    assert_eq!(EventMsg::AttachFrame.encode(), [0]);
    assert_eq!(
        EventMsg::PaintReady { seq: 42 }.encode(),
        [1, 42, 0, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(
        EventMsg::LoginSubmitted {
            origin: "o".to_owned(),
            body: "{}".to_owned(),
        }
        .encode(),
        [22, 1, 0, 0, 0, b'o', 2, 0, 0, 0, b'{', b'}']
    );
}
