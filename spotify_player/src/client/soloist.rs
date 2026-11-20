//! The Spotify Soloist device running on this machine

use std::borrow::Cow;

/// Name of the Spotify Connect device of a Soloist running on this machine: its `-n` argument
pub fn local_device_name() -> Option<String> {
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(Result::ok)
        .filter(|process| {
            std::fs::read_to_string(process.path().join("comm"))
                .is_ok_and(|comm| comm.trim_end() == "soloist")
        })
        .find_map(|process| device_name(&std::fs::read(process.path().join("cmdline")).ok()?))
}

/// The device name in a Soloist command line (NUL-separated arguments), if it's the player's
fn device_name(cmdline: &[u8]) -> Option<String> {
    let mut args = cmdline.split(|&b| b == 0).map(String::from_utf8_lossy);
    while let Some(arg) = args.next() {
        if arg == "-n" || arg == "--device-name" {
            return args.next().map(Cow::into_owned);
        }
        if let Some(name) = arg.strip_prefix("--device-name=") {
            return Some(name.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::device_name;

    #[test]
    fn reads_the_device_name_argument() {
        let player = b"/opt/soloist/soloist\0-n\0box-spot\0-k\0spak_x\0-w\x00127.0.0.1:0\0";
        assert_eq!(device_name(player).as_deref(), Some("box-spot"));
        let long = b"soloist\0--device-name=Living room\0";
        assert_eq!(device_name(long).as_deref(), Some("Living room"));
        // `soloist ctl ...` runs as `soloist` too, without a device
        assert_eq!(device_name(b"soloist\0ctl\0trace\0"), None);
    }
}
