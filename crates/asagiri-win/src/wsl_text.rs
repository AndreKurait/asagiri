//! WSL diagnostics use UTF-8 when requested; older launchers can emit UTF-16.

pub fn utf8(command: &mut std::process::Command) {
    command.env("WSL_UTF8", "1");
}

pub fn decode(bytes: &[u8]) -> String {
    if let Some(text) = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8_lossy(text).into_owned();
    }
    let (text, big_endian) = if let Some(text) = bytes.strip_prefix(&[0xff, 0xfe]) {
        (text, false)
    } else if let Some(text) = bytes.strip_prefix(&[0xfe, 0xff]) {
        (text, true)
    } else if bytes.contains(&0) {
        // Do not inspect only the first ASCII characters: localized UTF-16
        // messages can start with nonzero high bytes before any ASCII text.
        (bytes, false)
    } else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let mut chunks = text.chunks_exact(2);
    let units: Vec<u16> = chunks.by_ref().map(|pair| {
        if big_endian { u16::from_be_bytes([pair[0], pair[1]]) } else { u16::from_le_bytes([pair[0], pair[1]]) }
    }).collect();
    let mut output = String::from_utf16_lossy(&units).trim_end_matches('\0').to_owned();
    if !chunks.remainder().is_empty() { output.push(char::REPLACEMENT_CHARACTER); }
    output
}

pub fn missing_component(stdout: &[u8], stderr: &[u8]) -> bool {
    [stdout, stderr].iter().any(|text| decode(text).contains("WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_utf8_applies_only_to_the_owned_command() {
        let mut command = std::process::Command::new("wsl.exe");
        command.arg("--status").env("WSL_UTF8", "0");
        utf8(&mut command);
        assert_eq!(command.get_program(), "wsl.exe");
        assert_eq!(command.get_args().collect::<Vec<_>>(), ["--status"]);
        assert_eq!(command.get_envs().find(|(key, _)| *key == "WSL_UTF8").unwrap().1, Some(std::ffi::OsStr::new("1")));
    }

    #[test]
    fn localized_utf8_and_legacy_utf16_keep_the_original_text() {
        for text in [
            "Для этого приложения требуется дополнительный компонент Windows для Linux.\r\n",
            "需要启用适用于 Linux 的 Windows 子系统。\r\n",
            "Windows の Linux コンポーネントが必要です。\r\n",
            "يلزم تمكين مكوّن Windows لـ Linux.\r\n",
            "Ubuntu\r\n容器🐧\r\n",
        ] {
            assert_eq!(decode(text.as_bytes()), text);
            let wide: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
            assert_eq!(decode(&wide), text);
            let mut bom = vec![0xff, 0xfe];
            bom.extend(&wide);
            assert_eq!(decode(&bom), text);
        }
    }

    #[test]
    fn byte_order_marks_and_incomplete_output_do_not_garble_or_panic() {
        let text = "容器🐧";
        let mut wide = vec![0xfe, 0xff];
        wide.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        assert_eq!(decode(&wide), text);
        let mut utf8 = vec![0xef, 0xbb, 0xbf];
        utf8.extend(text.as_bytes());
        assert_eq!(decode(&utf8), text);
        assert_eq!(decode(b""), "");
        assert_eq!(decode(b"A\0B"), "A�");
        assert_eq!(decode(&[0xff, 0xfe, 0x3d, 0xd8]), "�");
    }

    #[test]
    fn missing_component_is_recognized_in_either_stream_and_encoding() {
        let text = "Для этого приложения требуется компонент.\r\nWsl/WSL_E_WSL_OPTIONAL_COMPONENT_REQUIRED\r\n";
        let wide: Vec<_> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        for stream in [text.as_bytes(), wide.as_slice()] {
            assert!(missing_component(stream, b""));
            assert!(missing_component(b"", stream));
        }
        assert!(!missing_component(b"Default Version: 2\r\n", b""));
        assert!(!missing_component(b"", b"Wsl/WSL_E_DEFAULT_DISTRO_NOT_FOUND\r\n"));
    }
}
