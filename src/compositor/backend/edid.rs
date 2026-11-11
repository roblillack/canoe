//! Who made a monitor and which one it is, from its EDID: make, model and
//! serial number. These name the monitor in wlr-output-management (tools
//! like kanshi match on them) and in canoe's own output profiles, where the
//! connector name alone would mix up monitors plugged into the same port.

use std::path::Path;

use smithay::reexports::drm::control::{connector, Device as ControlDevice};

/// Where the PNP vendor IDs and names are, as hwdata installs them.
const PNP_IDS: &[&str] = &["/usr/share/hwdata/pnp.ids", "/usr/share/misc/pnp.ids"];

/// What a monitor tells about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The vendor's name, or its three-letter PNP ID when the name is
    /// unknown.
    pub make: String,
    /// The monitor name descriptor, or the product code as `0x1234`.
    pub model: String,
    /// The serial number descriptor, or the numeric serial as `0x12345678`,
    /// if there is either.
    pub serial: Option<String>,
}

/// The identity of the monitor on `connector`, if it has a usable EDID.
pub fn read(device: &impl ControlDevice, connector: connector::Handle) -> Option<Identity> {
    let props = device.get_properties(connector).ok()?;
    let (handles, values) = props.as_props_and_values();
    let blob = handles.iter().zip(values).find_map(|(&handle, &value)| {
        let info = device.get_property(handle).ok()?;
        (info.name().to_bytes() == b"EDID").then_some(value)
    })?;
    if blob == 0 {
        return None;
    }
    let edid = device.get_property_blob(blob).ok()?;
    let mut identity = parse(&edid)?;
    if let Some(name) = PNP_IDS
        .iter()
        .find_map(|path| vendor_name(Path::new(path), &identity.make))
    {
        identity.make = name;
    }
    Some(identity)
}

/// Make (as the PNP ID), model and serial number from an EDID base block.
pub fn parse(edid: &[u8]) -> Option<Identity> {
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let vendor = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| {
        let code = ((vendor >> shift) & 0x1f) as u8;
        (1..=26)
            .contains(&code)
            .then_some((b'A' + code - 1) as char)
    };
    let make: String = [letter(10)?, letter(5)?, letter(0)?].iter().collect();
    let product = u16::from_le_bytes([edid[10], edid[11]]);
    let serial_number = u32::from_le_bytes([edid[12], edid[13], edid[14], edid[15]]);

    let mut name = None;
    let mut serial = None;
    for descriptor in edid[54..126].as_chunks::<18>().0 {
        // Display descriptors start with three zero bytes, timings don't.
        if descriptor[..3] != [0, 0, 0] {
            continue;
        }
        let text = || descriptor_text(&descriptor[5..18]);
        match descriptor[3] {
            0xfc => name = name.or_else(text),
            0xff => serial = serial.or_else(text),
            _ => {}
        }
    }
    Some(Identity {
        make,
        model: name.unwrap_or_else(|| format!("0x{product:04X}")),
        serial: serial.or_else(|| (serial_number != 0).then(|| format!("0x{serial_number:08X}"))),
    })
}

/// The text of a descriptor: up to a line feed, without padding, `None` if
/// that leaves nothing.
fn descriptor_text(bytes: &[u8]) -> Option<String> {
    let end = bytes
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(bytes.len());
    let text: String = bytes[..end]
        .iter()
        .map(|&b| {
            if b.is_ascii_graphic() || b == b' ' {
                b as char
            } else {
                '?'
            }
        })
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// The vendor name for PNP ID `id`, from a `pnp.ids` file (`ID<TAB>Name`
/// per line).
fn vendor_name(path: &Path, id: &str) -> Option<String> {
    let contents = std::fs::read_to_string(path).ok()?;
    contents.lines().find_map(|line| {
        let (code, name) = line.split_once('\t')?;
        (code == id)
            .then(|| name.trim().to_string())
            .filter(|n| !n.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An EDID base block for vendor `DEL`, product 0xA0B1, numeric serial
    /// 0x01020304 and the given descriptors (tag, text).
    fn edid(descriptors: &[(u8, &str)]) -> Vec<u8> {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00]);
        // D=4, E=5, L=12
        let vendor: u16 = (4 << 10) | (5 << 5) | 12;
        edid[8..10].copy_from_slice(&vendor.to_be_bytes());
        edid[10..12].copy_from_slice(&0xA0B1u16.to_le_bytes());
        edid[12..16].copy_from_slice(&0x01020304u32.to_le_bytes());
        // First descriptor: a detailed timing (non-zero pixel clock).
        edid[54] = 0x01;
        edid[55] = 0x1d;
        for (i, (tag, text)) in descriptors.iter().enumerate() {
            let start = 72 + 18 * i;
            edid[start + 3] = *tag;
            let mut field = [b' '; 13];
            let bytes = text.as_bytes();
            field[..bytes.len()].copy_from_slice(bytes);
            if bytes.len() < 13 {
                field[bytes.len()] = b'\n';
            }
            edid[start + 5..start + 18].copy_from_slice(&field);
        }
        edid
    }

    #[test]
    fn reads_name_and_serial_descriptors() {
        let identity = parse(&edid(&[(0xff, "5KXT123"), (0xfc, "DELL U2723QE")])).unwrap();
        assert_eq!(
            identity,
            Identity {
                make: "DEL".into(),
                model: "DELL U2723QE".into(),
                serial: Some("5KXT123".into()),
            }
        );
    }

    #[test]
    fn falls_back_to_codes_without_descriptors() {
        let identity = parse(&edid(&[])).unwrap();
        assert_eq!(identity.model, "0xA0B1");
        assert_eq!(identity.serial.as_deref(), Some("0x01020304"));
    }

    #[test]
    fn full_length_descriptor_has_no_line_feed() {
        let identity = parse(&edid(&[(0xfc, "ABCDEFGHIJKLM")])).unwrap();
        assert_eq!(identity.model, "ABCDEFGHIJKLM");
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse(&[0u8; 128]), None);
        assert_eq!(parse(&edid(&[])[..100]), None);
    }

    #[test]
    fn looks_up_vendor_names() {
        let dir = std::env::temp_dir().join(format!("canoe-pnp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("pnp.ids");
        std::fs::write(&path, "DEC\tDigital\nDEL\tDell Inc.\n").unwrap();
        assert_eq!(vendor_name(&path, "DEL").as_deref(), Some("Dell Inc."));
        assert_eq!(vendor_name(&path, "XYZ"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
