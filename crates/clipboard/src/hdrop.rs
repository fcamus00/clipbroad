//! The Windows `CF_HDROP` format: what Explorer puts on the clipboard when files are copied.
//!
//! The data is a `DROPFILES` header followed by the paths, each ending in a NUL, and one more NUL
//! after the last path:
//!
//! | Offset | Field    | Meaning                                         |
//! |--------|----------|-------------------------------------------------|
//! | 0      | `pFiles` | where the paths start, from the start of the data |
//! | 4      | `pt`     | the drop point (two `LONG`s); unused here       |
//! | 12     | `fNC`    | whether `pt` is in the non-client area; unused  |
//! | 16     | `fWide`  | non-zero: the paths are UTF-16, else ANSI       |
//!
//! Paths are kept as UTF-16 units, the way Windows stores them, so this module is plain Rust and
//! its tests run on every platform.

/// Size of the `DROPFILES` header, where our paths start.
const HEADER_LEN: u32 = 20;

/// The `CF_HDROP` data for `paths` (UTF-16, as `encode_wide` gives them), or `None` if a path is
/// empty or holds a NUL: either would end the list early.
pub fn encode(paths: &[Vec<u16>]) -> Option<Vec<u8>> {
    if paths
        .iter()
        .any(|path| path.is_empty() || path.contains(&0))
    {
        return None;
    }
    // `pFiles`, `pt` (two zeros), `fNC` = FALSE, `fWide` = TRUE.
    let header = [HEADER_LEN, 0, 0, 0, 1];
    let units = paths
        .iter()
        .flat_map(|path| path.iter().copied().chain([0]));
    Some(
        header
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .chain(units.chain([0]).flat_map(u16::to_le_bytes))
            .collect(),
    )
}

/// The paths in `CF_HDROP` data as UTF-16 units, or `None` if it holds none or is malformed. The
/// data may run past the final NUL (a clipboard memory block can be bigger than its content).
///
/// ANSI paths (`fWide` = FALSE, from very old apps) are read only if they are plain ASCII: any
/// other byte depends on the system code page.
pub fn decode(data: &[u8]) -> Option<Vec<Vec<u16>>> {
    let field = |offset: usize| {
        data.get(offset..)?
            .first_chunk::<4>()
            .copied()
            .map(u32::from_le_bytes)
    };
    let start = usize::try_from(field(0)?).ok()?;
    let wide = field(16)? != 0;
    let list = data.get(start..)?;
    let units: Vec<u16> = if wide {
        let (pairs, _odd_byte) = list.as_chunks::<2>();
        pairs.iter().copied().map(u16::from_le_bytes).collect()
    } else {
        list.iter().map(|&byte| u16::from(byte)).collect()
    };
    let mut paths = Vec::new();
    let mut rest = units.as_slice();
    loop {
        // No NUL before the end: the list is cut short.
        let end = rest.iter().position(|&unit| unit == 0)?;
        if end == 0 {
            break;
        }
        let path = &rest[..end];
        if !wide && !path.iter().all(|&unit| unit < 0x80) {
            return None;
        }
        paths.push(path.to_vec());
        rest = &rest[end + 1..];
    }
    (!paths.is_empty()).then_some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(path: &str) -> Vec<u16> {
        path.encode_utf16().collect()
    }

    #[test]
    fn encoded_paths_decode_back() {
        let paths = vec![
            wide(r"C:\Users\Ana\a b.txt"),
            wide(r"C:\Temp\ñandú 🦀.txt"),
            wide(r"\\server\share\x"),
        ];
        let data = encode(&paths).expect("valid paths");
        // The header, then each path and its NUL, then the final NUL.
        let units: usize = paths.iter().map(|path| path.len() + 1).sum::<usize>() + 1;
        assert_eq!(data.len(), 20 + units * 2);
        assert_eq!(data[..4], 20_u32.to_le_bytes());
        assert_eq!(data[16..20], 1_u32.to_le_bytes());
        assert_eq!(decode(&data), Some(paths.clone()));

        // Extra bytes after the list (a bigger memory block) are ignored.
        let mut padded = data;
        padded.extend([0xAB; 7]);
        assert_eq!(decode(&padded), Some(paths));
    }

    #[test]
    fn paths_that_would_end_the_list_early_are_not_encoded() {
        assert_eq!(encode(&[wide(r"C:\a"), Vec::new()]), None);
        assert_eq!(encode(&[wide("C:\\a\0b")]), None);
    }

    /// `CF_HDROP` data with the paths at `start` and the given `fWide`.
    fn hdrop(start: u32, wide: u32, list: &[u8]) -> Vec<u8> {
        let mut data: Vec<u8> = [start, 0, 0, 0, wide]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        data.resize(usize::try_from(start).expect("small offset"), 0);
        data.extend(list);
        data
    }

    #[test]
    fn reads_ascii_ansi_paths_and_a_later_start() {
        let data = hdrop(24, 0, b"C:\\a.txt\0D:\\b\0\0");
        assert_eq!(decode(&data), Some(vec![wide(r"C:\a.txt"), wide(r"D:\b")]));
    }

    #[test]
    fn malformed_or_empty_lists_are_no_files() {
        for (name, data) in [
            ("empty list", hdrop(20, 1, &[0, 0])),
            ("short header", vec![20, 0, 0, 0]),
            ("start past the end", hdrop(40, 1, &[])[..20].to_vec()),
            ("no final NUL", hdrop(20, 1, &[b'C', 0, b':', 0])),
            ("cut after a path", hdrop(20, 1, &[b'C', 0, 0, 0])),
            ("non-ASCII ANSI", hdrop(20, 0, b"C:\\\xf1\0\0")),
        ] {
            assert_eq!(decode(&data), None, "{name}");
        }
    }
}
