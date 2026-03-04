//! ISOBMFF (MP4) box parsing and manipulation utilities.
//! Centralizes boilerplate for traversing MP4 structures in memory.

/// Walk all top-level boxes in a buffer, and recursively traverse specified container boxes.
/// `callback` is invoked for EVERY box in pre-order traversal.
/// The callback signature is `|box_type: &[u8; 4], payload: &[u8]|`.
/// Mutable version of `walk_boxes`.
/// `callback` is invoked for EVERY box in pre-order traversal, with a mutable payload slice.
pub fn walk_boxes_mut<F>(data: &mut [u8], containers: &[&[u8; 4]], callback: &mut F)
where
    F: FnMut(&[u8; 4], &mut [u8]),
{
    let mut pos = 0;
    let len = data.len();
    while pos + 8 <= len {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if size < 8 || pos + size > len {
            break;
        }
        let btype: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();

        let payload = &mut data[pos + 8..pos + size];
        callback(&btype, payload);

        if containers.contains(&&btype) {
            walk_boxes_mut(payload, containers, callback);
        }

        pos += size;
    }
}
