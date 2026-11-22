//! The freedesktop "ReserveDevice1" protocol: how audio programs ask each other to let go of a sound card.
//! WirePlumber owns `org.freedesktop.ReserveDevice1.Audio<N>` for each card it manages; we ask it to
//! release the card, then hold the name ourselves while playing so it cannot grab the card back.
use std::sync::Mutex;
use std::time::Duration;
use zbus::blocking::Connection;
use zbus::fdo::{RequestNameFlags, RequestNameReply};
use zbus::interface;

const PRIORITY: i32 = 100; // above PipeWire/PulseAudio, so a release request from us wins

struct Reservation;

#[interface(name = "org.freedesktop.ReserveDevice1")]
impl Reservation {
    /// Someone else wants the card while we are playing: refuse.
    fn request_release(&self, _priority: i32) -> bool {
        false
    }

    #[zbus(property)]
    fn priority(&self) -> i32 {
        PRIORITY
    }

    #[zbus(property)]
    fn application_name(&self) -> &str {
        "rex-music"
    }

    #[zbus(property)]
    fn application_device_name(&self) -> &str {
        "rex-music output"
    }
}

static HELD: Mutex<Vec<(Connection, String)>> = Mutex::new(Vec::new());

/// Bus name and object path for a card index.
pub fn names(card: &str) -> (String, String) {
    (format!("org.freedesktop.ReserveDevice1.Audio{card}"), format!("/org/freedesktop/ReserveDevice1/Audio{card}"))
}

fn claim_blocking(card: &str) -> bool {
    let (name, path) = names(card);
    let Ok(conn) = Connection::session() else { return false };
    if conn.object_server().at(path.as_str(), Reservation).is_err() { return false; }

    // The owner releases asynchronously after saying yes, so retry the request a few times.
    for attempt in 0..6 {
        match conn.request_name_with_flags(name.as_str(), RequestNameFlags::DoNotQueue.into()) {
            Ok(RequestNameReply::PrimaryOwner) | Ok(RequestNameReply::AlreadyOwner) => {
                HELD.lock().unwrap_or_else(|e| e.into_inner()).push((conn, name));
                return true;
            }
            Ok(_) | Err(_) => {
                if attempt == 0 {
                    let said_yes = conn
                        .call_method(Some(name.as_str()), path.as_str(), Some("org.freedesktop.ReserveDevice1"), "RequestRelease", &(PRIORITY,))
                        .ok()
                        .and_then(|m| m.body().deserialize::<bool>().ok())
                        .unwrap_or(false);
                    if !said_yes { return false; }
                }
                std::thread::sleep(Duration::from_millis(150));
            }
        }
    }
    false
}

/// Try to take the card over D-Bus; gives up after a few seconds so playback never waits on the bus.
pub fn claim(card: &str) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    let card = card.to_string();
    std::thread::spawn(move || { let _ = tx.send(claim_blocking(&card)); });
    rx.recv_timeout(Duration::from_secs(4)).unwrap_or(false)
}

/// Let the card go: PipeWire may use it again.
pub fn release_all() {
    for (conn, name) in HELD.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        let _ = conn.release_name(name.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_name_and_path_follow_the_spec() {
        let (name, path) = names("1");
        assert_eq!(name, "org.freedesktop.ReserveDevice1.Audio1");
        assert_eq!(path, "/org/freedesktop/ReserveDevice1/Audio1");
    }
}
