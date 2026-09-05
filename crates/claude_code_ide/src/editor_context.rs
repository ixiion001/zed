//! Small editor primitives shared by the protocol adapters. No wire JSON here.
use gpui::App;
use language::Buffer;
use std::ops::Range;
use text::{Point, PointUtf16};

/// A local file identity; untitled and remote buffers have none.
pub(crate) fn local_abs_path(buffer: &Buffer, cx: &App) -> Option<String> {
    buffer
        .file()
        .and_then(|file| file.as_local())
        .map(|file| file.abs_path(cx).to_string_lossy().into_owned())
}

/// Zed columns count UTF-8 bytes; native IDE protocols count UTF-16 code units.
pub(crate) fn utf16_range(
    snapshot: &editor::MultiBufferSnapshot,
    range: Range<Point>,
) -> (PointUtf16, PointUtf16) {
    (
        snapshot.point_to_point_utf16(range.start),
        snapshot.point_to_point_utf16(range.end),
    )
}

/// Copy at most `limit` bytes, even when a rope chunk is much larger.
pub(crate) fn bounded_text<'a>(
    chunks: impl IntoIterator<Item = &'a str>,
    limit: usize,
) -> (String, bool) {
    let mut text = String::new();
    for chunk in chunks {
        let remaining = limit - text.len();
        if chunk.len() > remaining {
            text.push_str(&chunk[..chunk.floor_char_boundary(remaining)]);
            return (text, true);
        }
        text.push_str(chunk);
    }
    (text, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    #[test]
    fn bounded_text_preserves_unicode_and_stops_reading_chunks() {
        let mut chunks_read = 0;
        let chunks = ["é", "😀世界", "never copied"]
            .into_iter()
            .inspect(|_| chunks_read += 1);
        assert_eq!(bounded_text(chunks, 7), ("é😀".into(), true));
        assert_eq!(chunks_read, 2);
        assert_eq!(bounded_text(["é", "😀"], 6), ("é😀".into(), false));
        assert_eq!(bounded_text(["😀"], 0), (String::new(), true));
    }

    #[gpui::test]
    fn live_unsaved_buffer_uses_utf16_coordinates(cx: &mut gpui::TestAppContext) {
        let buffer = cx.new(|cx| Buffer::local("é😀世界\nnext", cx));
        let multi = cx.new(|cx| editor::MultiBuffer::singleton(buffer.clone(), cx));
        multi.read_with(cx, |multi, cx| {
            let snapshot = multi.snapshot(cx);
            let range = Point::new(0, 6)..Point::new(0, 12);
            let (start, end) = utf16_range(&snapshot, range.clone());
            assert_eq!((start.row, start.column, end.column), (0, 3, 5));
            assert_eq!(
                bounded_text(snapshot.text_for_range(range), 200_000).0,
                "世界"
            );
        });
        buffer.update(cx, |buffer, cx| {
            buffer.edit([(6..12, "編集")], None, cx);
        });
        multi.read_with(cx, |multi, cx| {
            let snapshot = multi.snapshot(cx);
            assert_eq!(
                bounded_text(
                    snapshot.text_for_range(Point::new(0, 6)..Point::new(0, 12)),
                    200_000
                )
                .0,
                "編集"
            );
        });
    }
}
