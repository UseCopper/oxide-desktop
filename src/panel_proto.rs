//! The wire format between the compositor and its panel, and nothing else.
//!
//! The panel is a separate process, so the two halves of this protocol are the only
//! thing they share. Both live in one module deliberately: a framing rule that the
//! writer and the reader each keep their own copy of is a framing rule that will one
//! day disagree, and when it does the symptom is a panel that has silently stopped
//! understanding the compositor — no error, no crash, just a bar that never updates
//! again.
//!
//! # Framing
//!
//! Every message, in both directions, is a length-prefixed frame:
//!
//! ```text
//! <decimal byte count> '\n' <exactly that many bytes>
//! ```
//!
//! One rule for text and binary alike. That is the whole point of the format.
//!
//! The previous wire format mixed two schemes on one stream: text messages were
//! newline-terminated, while a preview was a header line followed by a pixel count.
//! Raw RGBA contains `0x0a`, so the payload could not be newline-terminated — hence
//! two schemes, hence a reader that had to remember whether it was mid-message. The
//! compositor's writer could be interrupted part-way through a payload (the socket
//! filled up), and when a new window list then arrived it cleared the queue, throwing
//! away the rest of the pixels. The reader on the other end still believed a payload
//! was owed, and consumed everything after it — including every future window list —
//! as image data. Because a window list has to start with the literal `list\t`, no
//! later one could ever parse again. The panel was dead until restarted.
//!
//! A length prefix cannot fail that way. A frame is either whole or absent, so a
//! writer may always discard, reorder or partially transmit whatever it likes: the
//! reader resynchronises by construction, and the worst a dropped frame can do is
//! lose one message.
//!
//! # What is in a frame
//!
//! Text frames are one tab-separated line, with no trailing newline (the frame length
//! already ends it). Image frames are a header line, a newline, then exactly
//! `width * height * 4` raw bytes — the one inner newline is unambiguous because the
//! frame length is already known, so no field of it can be confused for a delimiter.
//!
//! No field may contain a tab or a newline. [`sanitize`] enforces that on the way out,
//! and [`parse_text`] refuses anything that arrives with one rather than guessing.

use std::collections::VecDeque;

/// Largest frame accepted, in bytes.
///
/// A preview at the panel's own request size (360x118 RGBA) is about 170 KB, so this
/// leaves a wide margin for a high-DPI screen while refusing to let a malformed or
/// hostile header commit the compositor or the panel to an enormous allocation. The
/// reader does not wait for the declared length before applying this: it is checked
/// against the header, which is available immediately.
pub const MAX_FRAME_BYTES: usize = 8 << 20;

/// Largest image dimension accepted, in pixels, on either side.
///
/// Guards the one request that could otherwise ask the compositor for a readback
/// costing tens of gigabytes: `preview` takes its dimensions from the panel, and a
/// compositor that renders any size it is asked for will honour a large one. Anything
/// larger is refused rather than clamped, because silently returning an image at a
/// different size than the header claims would reintroduce a length mismatch.
pub const MAX_IMAGE_DIM: i32 = 512;

/// Most windows a window list may describe.
pub const MAX_LIST_WINDOWS: usize = 1024;

/// Most fields a text frame may carry.
///
/// The longest legitimate frame is a window list: a verb, a count, and then five fields
/// per window — id, focused, minimized, app id, title. This bound is derived from
/// [`MAX_LIST_WINDOWS`] rather than picked, because a bound set too low is not a
/// convenience: a frame over it is dropped whole, and a window list is dropped *and
/// every one after it*, leaving the bar frozen at whatever it last saw. The last value
/// here allowed fourteen windows and rejected fifteen.
pub const MAX_TEXT_FIELDS: usize = MAX_LIST_WINDOWS * 5 + 2;

/// Longest single field, in bytes.
///
/// A window title is whatever a client passed to `set_title`, so it is as long as the
/// client likes. The panel measures titles with Pango to ellipsize them, which it does
/// per repaint, so an unbounded title is quadratic work on the panel's main loop. The
/// compositor truncates on the way out and the panel treats an over-long field from any
/// source the same way.
pub const MAX_FIELD_BYTES: usize = 1024;

/// Why a frame could not be accepted.
///
/// Every case here means the *stream* is untrustworthy rather than one message being
/// merely unwanted. A length prefix that overflows, a frame larger than the ceiling or
/// a header with a field count past the bound all say the peer is not speaking this
/// protocol, and there is no way to recover: the reader's position in the stream would
/// be a guess. So these are reported rather than skipped, and the caller drops the
/// connection. A message that is merely *malformed* — a bad number, a missing field —
/// is not this; see [`parse_text`], which returns `None` and leaves the stream intact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// The length prefix was not a plain decimal number within [`MAX_FRAME_BYTES`].
    BadLength,
    /// A frame header declared more bytes than [`MAX_FRAME_BYTES`].
    TooLarge,
    /// The peer closed the connection cleanly.
    Closed,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadLength => f.write_str("malformed frame length"),
            FrameError::TooLarge => f.write_str("frame exceeds the maximum size"),
            FrameError::Closed => f.write_str("peer closed the connection"),
        }
    }
}

/// Strip the characters that could be mistaken for structure, in place.
///
/// A tab or a newline in a title would end the field or the frame it sits in, so every
/// field passes through here on the way out. Replaced rather than dropped: a title
/// with a tab in it should still read as the title, just without the tab.
///
/// Does *not* truncate. Callers that want a length bound apply [`truncate`] as well;
/// keeping the two separate means the framing guarantee does not depend on a caller
/// remembering to do it.
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '\t' || c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// Shorten a field to [`MAX_FIELD_BYTES`], never splitting a character.
pub fn truncate(value: &str) -> &str {
    if value.len() <= MAX_FIELD_BYTES {
        return value;
    }
    let mut end = MAX_FIELD_BYTES;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Split a text frame into its fields.
///
/// `None` if the frame has more fields than [`MAX_TEXT_FIELDS`], or holds a tab where
/// there should be one. Deliberately *not* an error: the frame length was honoured, so
/// the stream is still positioned correctly for the next one, and skipping one
/// unwanted message costs nothing.
pub fn parse_text(frame: &[u8]) -> Option<Vec<String>> {
    // No trailing newline: the frame length ended the frame. Tolerating one keeps a
    // hand-written or older peer from costing us the message.
    let frame = match frame.last() {
        Some(b'\n') => &frame[..frame.len() - 1],
        _ => frame,
    };
    let text = std::str::from_utf8(frame).ok()?;
    let fields: Vec<String> = text.split('\t').map(|field| field.to_string()).collect();
    if fields.len() > MAX_TEXT_FIELDS {
        return None;
    }
    // A raw tab cannot survive the split, so this can only be a control character that
    // `sanitize` was meant to remove. Refuse rather than silently accept a frame whose
    // fields mean something other than they appear to.
    if text.contains(['\n', '\r']) {
        return None;
    }
    Some(fields)
}

/// The image a preview frame carries: its window, its size, and its pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameImage {
    pub id: u64,
    pub width: i32,
    pub height: i32,
    pub pixels: Vec<u8>,
}

/// Build a preview frame: `img\t<id>\t<width>\t<height>\n` then the pixels.
///
/// Checks that the payload is exactly the size the header claims, so a caller cannot
/// produce a frame whose length and contents disagree — the same class of mistake the
/// framing exists to prevent.
pub fn encode_image(id: u64, width: i32, height: i32, pixels: &[u8]) -> Option<Vec<u8>> {
    let expected = image_len(width, height)?;
    if pixels.len() != expected {
        return None;
    }
    let body_len = expected + format!("img\t{id}\t{width}\t{height}\n").len();
    let mut frame = Vec::with_capacity(body_len + 12);
    frame.extend_from_slice(body_len.to_string().as_bytes());
    frame.push(b'\n');
    frame.extend_from_slice(format!("img\t{id}\t{width}\t{height}\n").as_bytes());
    frame.extend_from_slice(pixels);
    Some(frame)
}

/// Byte count of a `width` x `height` RGBA image, or `None` if the size is not one we
/// will send or accept.
///
/// The multiplication is checked rather than done in `usize` and hoped over: on a
/// 32-bit target two plausible-looking dimensions overflow silently, and the result is
/// then a length that disagrees with the pixels that follow it.
pub fn image_len(width: i32, height: i32) -> Option<usize> {
    if width <= 0 || height <= 0 || width > MAX_IMAGE_DIM || height > MAX_IMAGE_DIM {
        return None;
    }
    let len = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if len > MAX_FRAME_BYTES {
        return None;
    }
    Some(len)
}

/// Take the image out of a preview frame, if it is one and if it is whole.
pub fn decode_image(frame: &[u8]) -> Option<FrameImage> {
    let newline = frame.iter().position(|&b| b == b'\n')?;
    let fields = parse_text(&frame[..newline])?;
    if fields.first()? != "img" {
        return None;
    }
    // Verb, id, width, height. The byte count is not a field: the frame length already
    // said it, and repeating it would be a second thing to disagree with.
    if fields.len() != 4 {
        return None;
    }
    let id = fields[1].parse::<u64>().ok()?;
    let width = fields[2].parse::<i32>().ok()?;
    let height = fields[3].parse::<i32>().ok()?;
    let pixels = frame[newline + 1..].to_vec();
    if pixels.len() != image_len(width, height)? {
        return None;
    }
    Some(FrameImage {
        id,
        width,
        height,
        pixels,
    })
}

/// Wrap a text message as a frame: the length, a newline, then the bytes.
pub fn encode_text(message: &str) -> Vec<u8> {
    let mut frame = Vec::with_capacity(message.len() + 12);
    frame.extend_from_slice(message.len().to_string().as_bytes());
    frame.push(b'\n');
    frame.extend_from_slice(message.as_bytes());
    frame
}

/// Reads frames out of a byte stream that arrives in arbitrary chunks.
///
/// Holds a partial frame between calls, which is the only state it needs: a header
/// split across two reads is kept until its length is known, and a payload split across
/// ten reads is kept until it is whole.
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
    /// Bytes of the frame now being accumulated that have already arrived.
    ///
    /// While this is `None` the reader is looking for a length prefix; once a prefix
    /// has been read it waits for exactly this many payload bytes.
    wanted: Option<usize>,
}

impl FrameReader {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything, for a connection that is being replaced.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.wanted = None;
    }

    /// Bytes buffered but not yet made into a frame.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }

    /// Take delivery of bytes read from the socket.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole frame, or `None` if more bytes are needed.
    ///
    /// `Err` means the stream cannot be parsed, or has ended, and the caller should
    /// drop the connection.
    pub fn next_frame(&mut self) -> Option<Result<Vec<u8>, FrameError>> {
        if self.wanted.is_none() {
            match self.read_length() {
                // The prefix has not fully arrived.
                None => return None,
                // It arrived, and it is not one we can follow.
                Some(Err(err)) => return Some(Err(err)),
                Some(Ok(())) => {}
            }
        }
        // Set by `read_length`, which returns early only when it has not.
        let wanted = self.wanted.unwrap_or(0);

        if self.buffer.len() < wanted {
            return None;
        }
        let frame: Vec<u8> = self.buffer.drain(..wanted).collect();
        self.wanted = None;
        Some(Ok(frame))
    }

    /// Read one length prefix into [`Self::wanted`].
    ///
    /// `None` means the prefix has not fully arrived and the caller should wait for
    /// more bytes. `Some(Err)` means it arrived and is not one this stream can be read
    /// past.
    fn read_length(&mut self) -> Option<Result<(), FrameError>> {
        // A prefix is a run of decimal digits ended by a newline. Until that newline
        // arrives the bytes stay buffered: a chunk that stops halfway through "1234"
        // must not be mistaken for a complete prefix.
        // A run of digits longer than any length we would write is not one of ours.
        // Checked against what is buffered rather than against the gap to the newline,
        // so a peer that never sends a newline is caught too instead of growing the
        // buffer forever.
        let run = self
            .buffer
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(self.buffer.len());
        if run > 20 {
            return Some(Err(FrameError::BadLength));
        }
        let newline = self.buffer.iter().position(|&b| b == b'\n')?;
        let digits = &self.buffer[..newline];
        let parsed = std::str::from_utf8(digits)
            .ok()
            .filter(|text| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|text| text.parse::<usize>().ok());
        self.buffer.drain(..=newline);
        match parsed {
            // A zero-length frame is legal on the wire and meaningless in practice. It
            // is stored rather than returned here so that exactly one place in this
            // function turns a frame into something the caller can use.
            Some(len) if len <= MAX_FRAME_BYTES => self.wanted = Some(len),
            Some(_) => return Some(Err(FrameError::TooLarge)),
            None => return Some(Err(FrameError::BadLength)),
        }
        Some(Ok(()))
    }
}

/// Queues whole frames and hands them to a non-blocking socket a piece at a time.
///
/// The reason frames are kept whole here rather than concatenated into one byte buffer:
/// the writer has to be able to *discard* queued work — a new window list supersedes
/// the old one — and it has to be able to do that while the front frame is only part
/// transmitted. Keeping frames separate is what makes both possible. `written` records
/// how far into the front frame the kernel got, so a superseded frame is dropped whole
/// and a partly-sent one is never mistaken for a complete one.
#[derive(Debug, Default)]
pub struct FrameWriter {
    pending: VecDeque<Frame>,
    /// Bytes of all queued frames, so the backlog ceiling can be checked without
    /// walking the queue.
    queued: usize,
}

/// One frame on its way out.
#[derive(Debug)]
struct Frame {
    bytes: Vec<u8>,
    /// How much of it the kernel has already taken. Only the front frame is ever
    /// partial.
    written: usize,
    /// Whether this frame carries pixels, which decides whether a newer window list may
    /// discard it.
    is_image: bool,
}

impl FrameWriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes waiting to be handed to the kernel.
    pub fn queued(&self) -> usize {
        self.queued
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Queue a text frame at the back.
    pub fn push_text(&mut self, message: &str) {
        self.push_back(encode_text(message), false);
    }

    /// Queue a preview frame at the back.
    pub fn push_image(&mut self, bytes: Vec<u8>) {
        self.push_back(bytes, true);
    }

    fn push_back(&mut self, bytes: Vec<u8>, is_image: bool) {
        self.queued += bytes.len();
        self.pending.push_back(Frame {
            bytes,
            written: 0,
            is_image,
        });
    }

    /// Queue a frame ahead of everything not yet partly transmitted, superseding it.
    ///
    /// Used for the window list, which is what the panel needs in order to stay usable
    /// at all: a preview it has not been sent yet is stale the moment a window moves,
    /// and an older window list is stale the moment this one exists. So everything still
    /// queued goes, and the frame that supersedes it goes first.
    ///
    /// "Ahead of everything" is the subtlety. The frame at the front may already be part
    /// way into the socket, and its remaining bytes are owed to the reader, so the new
    /// frame goes *behind* that one rather than in front of it. Inserting in front would
    /// emit the tail of one frame between the head of another and its own body — the
    /// stream corruption the length prefix exists to make impossible.
    pub fn push_priority(&mut self, message: &str) {
        // Only *text* is superseded. A queued preview used to be dropped here too, on the
        // reasoning that it was stale the moment a window moved — but the sender had
        // already recorded that window as previewed, so the frame was thrown away and
        // never asked for again. The cell then stayed blank for as long as the menu stayed
        // open, which is exactly the "the previews never appear" symptom. A preview that
        // is genuinely too far behind is bounded by the backlog check where it is queued,
        // and a stale one is replaced by the next one a moment later anyway.
        self.drop_queued(|frame| !frame.is_image);
        let bytes = encode_text(message);
        self.queued += bytes.len();
        let position = usize::from(self.pending.front().is_some_and(|f| f.written > 0));
        self.pending.insert(position, Frame {
            bytes,
            written: 0,
            is_image: false,
        });
    }

    /// Drop queued frames for which `drop` says yes.
    ///
    /// Never drops the frame being transmitted, whatever the predicate says: the rest
    /// of it is already owed to the reader, and dropping it would leave the reader
    /// waiting for bytes that will never come.
    fn drop_queued(&mut self, mut drop: impl FnMut(&Frame) -> bool) {
        let transmitting = self.pending.front().is_some_and(|frame| frame.written > 0);
        let mut index = 0usize;
        self.pending.retain(|frame| {
            let keep = (transmitting && index == 0) || !drop(frame);
            index += 1;
            keep
        });
        self.recount();
    }

    /// Throw away everything queued that is not already partly transmitted.
    pub fn clear(&mut self) {
        self.drop_queued(|_| true);
    }

    /// Recompute the backlog from the frames that are actually still queued.
    ///
    /// Counting only what is *unsent* is what makes this number usable as a backpressure
    /// threshold: a frame the socket has half taken has cost the writer half its buffer
    /// already, and charging it in full would make the compositor drop good previews
    /// because of bytes the kernel has.
    fn recount(&mut self) {
        self.queued = self
            .pending
            .iter()
            .map(|frame| frame.bytes.len() - frame.written)
            .sum();
    }

    /// The next chunk to hand the socket, if anything is queued.
    pub fn next_chunk(&mut self) -> Option<&[u8]> {
        let frame = self.pending.front_mut()?;
        Some(&frame.bytes[frame.written..])
    }

    /// Record that the socket took `written` bytes of the front frame.
    pub fn advance(&mut self, written: usize) {
        let Some(frame) = self.pending.front_mut() else {
            return;
        };
        let taken = written.min(frame.bytes.len() - frame.written);
        frame.written += taken;
        self.queued -= taken;
        if frame.written == frame.bytes.len() {
            self.pending.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(reader: &mut FrameReader, bytes: &[u8]) -> Vec<Result<Vec<u8>, FrameError>> {
        reader.feed(bytes);
        let mut out = Vec::new();
        while let Some(frame) = reader.next_frame() {
            out.push(frame);
        }
        out
    }

    #[test]
    fn a_text_frame_survives_the_round_trip() {
        let frame = encode_text("list\t1\t1\t0\tfirefox\tA Tab");
        let mut reader = FrameReader::new();
        let frames = read_all(&mut reader, &frame);
        assert_eq!(frames, vec![Ok(b"list\t1\t1\t0\tfirefox\tA Tab".to_vec())]);
        assert_eq!(parse_text(&frames[0].as_ref().unwrap().clone()).unwrap().len(), 6);
    }

    #[test]
    fn a_frame_split_at_every_byte_still_reads_back_whole() {
        // The case the old format could not survive: a payload cut short mid-message.
        let frame = encode_text("list\t2\t1\t0\ta\tOne\t2\t0\tb\tTwo");
        for cut in 1..frame.len() {
            let mut reader = FrameReader::new();
            reader.feed(&frame[..cut]);
            let first = reader.next_frame();
            assert!(first.is_none(), "a partial frame was handed out at cut {cut}");
            reader.feed(&frame[cut..]);
            assert_eq!(
                reader.next_frame(),
                Some(Ok(b"list\t2\t1\t0\ta\tOne\t2\t0\tb\tTwo".to_vec())),
                "frame did not survive a split at {cut}"
            );
        }
    }

    #[test]
    fn a_frame_chunked_one_byte_at_a_time_arrives_intact() {
        let pixels: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        let frame = encode_image(7, 10, 10, &pixels).unwrap();
        let mut reader = FrameReader::new();
        let mut got = Vec::new();
        for byte in &frame {
            reader.feed(&[*byte]);
            while let Some(next) = reader.next_frame() {
                got.push(next.unwrap());
            }
        }
        assert_eq!(got.len(), 1);
        assert_eq!(decode_image(&got[0]).unwrap().pixels, pixels);
    }

    #[test]
    fn pixels_containing_newlines_do_not_end_the_frame() {
        // Every 4th byte is a newline, which is what RGBA regularly contains.
        let pixels: Vec<u8> = (0..64u8).map(|i| if i % 4 == 3 { b'\n' } else { i }).collect();
        let frame = encode_image(1, 4, 4, &pixels).unwrap();
        let mut reader = FrameReader::new();
        let frames = read_all(&mut reader, &frame);
        assert_eq!(frames.len(), 1);
        let image = decode_image(frames[0].as_ref().unwrap()).unwrap();
        assert_eq!(image.pixels, pixels);
        assert_eq!((image.id, image.width, image.height), (1, 4, 4));
    }

    #[test]
    fn two_frames_in_one_chunk_are_two_frames() {
        let mut wire = encode_text("focus\t3");
        wire.extend_from_slice(&encode_text("close\t3"));
        let mut reader = FrameReader::new();
        let frames = read_all(&mut reader, &wire);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], Ok(b"close\t3".to_vec()));
    }

    #[test]
    fn a_frame_at_the_ceiling_is_refused_by_its_header() {
        // Not by waiting for it: the length is known from the prefix, so nothing is
        // allocated and nothing is buffered waiting for bytes that would fill a
        // gigabyte.
        let mut reader = FrameReader::new();
        let huge = format!("{}\n", MAX_FRAME_BYTES + 1);
        reader.feed(huge.as_bytes());
        assert_eq!(reader.next_frame(), Some(Err(FrameError::TooLarge)));
    }

    #[test]
    fn a_malformed_length_is_reported_rather_than_guessed_at() {
        for bad in ["abc\n", "\n", "12x4\n", "-4\n", " 12\n", "+12\n"] {
            let mut reader = FrameReader::new();
            reader.feed(bad.as_bytes());
            assert_eq!(
                reader.next_frame(),
                Some(Err(FrameError::BadLength)),
                "{bad:?} should not parse"
            );
        }
    }

    #[test]
    fn a_reader_that_never_sees_a_newline_gives_up_rather_than_growing() {
        let mut reader = FrameReader::new();
        reader.feed(&[b'1'; 64]);
        assert_eq!(reader.next_frame(), Some(Err(FrameError::BadLength)));
    }

    #[test]
    fn an_image_whose_payload_does_not_match_its_header_is_refused() {
        // Both halves checked, so a frame can never describe pixels it does not carry.
        let pixels = vec![0u8; 4 * 4 * 4];
        assert!(encode_image(1, 4, 4, &pixels).is_some());
        assert!(encode_image(1, 4, 4, &pixels[..8]).is_none());
        // And a header that claims a size its payload does not have.
        let frame = b"img\t1\t4\t4\ntoo short".to_vec();
        assert!(decode_image(&frame).is_none());
        // Or claims a size beyond the ceiling.
        assert!(image_len(MAX_IMAGE_DIM + 1, 4).is_none());
        assert!(image_len(4, 0).is_none());
        assert!(image_len(-1, 4).is_none());
    }

    #[test]
    fn image_length_does_not_overflow_on_absurd_dimensions() {
        // Two values a peer can type, which on a 32-bit target multiply past `usize`.
        assert_eq!(image_len(i32::MAX, i32::MAX), None);
        assert_eq!(image_len(1 << 20, 1 << 20), None);
        // And the one that is merely legal.
        assert_eq!(image_len(4, 4), Some(64));
    }

    #[test]
    fn a_field_with_structure_in_it_cannot_reach_the_wire() {
        let mut fields = Vec::new();
        for field in ["evil\norder other", "tab\there", "carriage\rreturn", "# not a comment"] {
            fields.push(sanitize(field));
        }
        let frame = encode_text(&format!("list\t{}\t{}", fields.len(), fields.join("\t")));
        let mut reader = FrameReader::new();
        let frames = read_all(&mut reader, &frame);
        let parsed = parse_text(frames[0].as_ref().unwrap()).unwrap();
        assert_eq!(parsed.len(), fields.len() + 2);
        // The newline became a space rather than starting a second message, and the
        // tab did not become a new field.
        assert_eq!(parsed[2], "evil order other");
        assert_eq!(parsed[4], "carriage return");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        let long = "é".repeat(MAX_FIELD_BYTES);
        let cut = truncate(&long);
        assert!(cut.len() <= MAX_FIELD_BYTES);
        assert!(long.starts_with(cut));
        // A short field is handed back untouched, and borrowed rather than copied.
        let short = "firefox";
        assert!(std::ptr::eq(truncate(short), short));
    }

    #[test]
    fn a_window_list_of_any_plausible_size_still_fits_the_field_bound() {
        // The bound is derived from the window count, not picked. It was once `72`, which
        // is fourteen windows and fifteen fields — so a session with fifteen windows open
        // had its window list dropped, and every list after it, leaving the bar frozen at
        // whatever it last saw.
        assert!(MAX_TEXT_FIELDS >= 5 * MAX_LIST_WINDOWS + 2);
        for count in [0usize, 1, 14, 15, 100, MAX_LIST_WINDOWS] {
            let mut fields = vec!["list".to_string(), count.to_string()];
            for index in 0..count {
                fields.push((index + 1).to_string());
                fields.push("0".to_string());
                fields.push("0".to_string());
                fields.push(format!("org.example.App{index}"));
                fields.push("A window title".to_string());
            }
            assert!(
                parse_text(&fields.join("\t").into_bytes()).is_some(),
                "a list of {count} windows was refused"
            );
        }
        // And one past the ceiling is still refused rather than parsed.
        let mut fields = vec!["list".to_string(), (MAX_LIST_WINDOWS + 1).to_string()];
        for _ in 0..(MAX_LIST_WINDOWS + 1) {
            fields.extend(["1".to_string(), "0".to_string(), "0".to_string(), "a".to_string(), "t".to_string()]);
        }
        assert_eq!(parse_text(&fields.join("\t").into_bytes()), None);
    }

    #[test]
    fn a_frame_with_too_many_fields_is_dropped_without_losing_the_stream() {
        let many = (0..MAX_TEXT_FIELDS + 1)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\t");
        assert_eq!(parse_text(many.as_bytes()), None);
        // Which is a property of the message, not the stream: the next frame is still
        // readable, because the length prefix already said where it ended.
        let mut wire = encode_text(&many);
        wire.extend_from_slice(&encode_text("focus\t1"));
        let mut reader = FrameReader::new();
        let frames = read_all(&mut reader, &wire);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[1], Ok(b"focus\t1".to_vec()));
    }

    #[test]
    fn a_new_window_list_does_not_disturb_a_half_sent_preview() {
        // The bug this format exists to prevent, reproduced exactly: a preview is part
        // way to the panel, a window list arrives, and the panel must still be able to
        // read what follows.
        let pixels: Vec<u8> = (0..(4 * 4 * 4)).map(|i| i as u8).collect();
        let image = encode_image(1, 4, 4, &pixels).unwrap();

        let mut writer = FrameWriter::new();
        writer.push_image(image.clone());
        writer.push_image(encode_image(2, 4, 4, &pixels).unwrap());
        // The socket takes half of the first image, then refuses.
        let half = writer.next_chunk().unwrap().len() / 2;
        writer.advance(half);
        assert!(!writer.is_empty(), "the transmitted frame was discarded");

        // A window list arrives and takes priority.
        writer.push_priority("list\t2\t1\t0\ta\tA\t2\t0\tb\tB");

        // Everything the reader is sent, in order, from the point the socket accepted
        // the half frame above. Note the queued preview for window 2 survives: the window
        // list goes in front of it rather than in place of it.
        let mut wire = image[..half].to_vec();
        while let Some(chunk) = writer.next_chunk() {
            let taken = chunk.len();
            wire.extend_from_slice(chunk);
            writer.advance(taken);
        }
        assert!(writer.is_empty());

        // And it reads back as three whole frames, in order: the image the socket had
        // started, the newer window list, and the preview that was queued behind it.
        // Nothing is orphaned, and nothing the sender had recorded as sent is dropped.
        let mut reader = FrameReader::new();
        reader.feed(&wire);
        let mut got = Vec::new();
        while let Some(frame) = reader.next_frame() {
            got.push(frame.expect("a stream this writer produced is readable"));
        }
        assert_eq!(got.len(), 3);
        assert_eq!(decode_image(&got[0]).unwrap().id, 1);
        assert_eq!(got[1], b"list\t2\t1\t0\ta\tA\t2\t0\tb\tB".to_vec());
        assert_eq!(decode_image(&got[2]).unwrap().id, 2);
    }

    #[test]
    fn a_superseded_window_list_drops_the_queued_previews_but_not_the_sent_one() {
        let pixels: Vec<u8> = (0..(4 * 4 * 4)).map(|i| i as u8).collect();
        let mut writer = FrameWriter::new();
        writer.push_image(encode_image(1, 4, 4, &pixels).unwrap());
        writer.push_image(encode_image(2, 4, 4, &pixels).unwrap());
        writer.push_text("list\t0");

        let whole = writer.next_chunk().unwrap().len();
        // Only one byte of the first frame reached the kernel.
        writer.advance(1);

        writer.push_priority("list\t1");
        // The image at the front is still there, mid-transmission — one byte of it already
        // sent — and the window list jumps the queued ones rather than displacing them.
        // Only the *stale window list* is superseded: a queued preview is not, because the
        // sender has already recorded that window as sent and will not ask again.
        let first = encode_image(1, 4, 4, &pixels).unwrap();
        let second = encode_image(2, 4, 4, &pixels).unwrap();
        assert_eq!(
            writer.queued(),
            // The rest of the first image, the preview behind it, and the new window list
            // in front of both.
            first.len() - 1 + second.len() + encode_text("list\t1").len()
        );
        assert_eq!(
            writer.next_chunk().unwrap().len(),
            whole - 1,
            "the partly-transmitted frame was dropped"
        );
    }

    #[test]
    fn clearing_never_abandons_bytes_the_socket_already_has() {
        let pixels: Vec<u8> = (0..(4 * 4 * 4)).map(|i| i as u8).collect();
        let image = encode_image(1, 4, 4, &pixels).unwrap();
        let mut writer = FrameWriter::new();
        writer.push_image(image.clone());
        writer.push_image(encode_image(2, 4, 4, &pixels).unwrap());
        writer.advance(5);
        writer.clear();
        // Five bytes of that frame are in the socket; the rest is still owed, so the
        // reader — which has already counted those five — is not left waiting forever.
        assert!(!writer.is_empty());
        assert_eq!(writer.next_chunk().unwrap().len(), image.len() - 5);
        assert_eq!(writer.queued(), image.len() - 5);
    }

    #[test]
    fn the_writer_accounts_for_its_own_backlog() {
        let mut writer = FrameWriter::new();
        assert!(writer.is_empty());
        assert_eq!(writer.queued(), 0);
        writer.push_text("focus\t1");
        assert_eq!(writer.queued(), encode_text("focus\t1").len());
        writer.next_chunk().map(|c| c.len()).map(|n| writer.advance(n));
        assert!(writer.is_empty());
        assert_eq!(writer.queued(), 0);
    }

    #[test]
    fn a_reset_reader_forgets_a_partial_frame() {
        let mut reader = FrameReader::new();
        let frame = encode_image(1, 4, 4, &vec![0u8; 64]).unwrap();
        reader.feed(&frame[..10]);
        assert!(reader.next_frame().is_none());
        reader.reset();
        assert_eq!(reader.buffered(), 0);
        // And the new connection's first frame reads cleanly, with no leftover from
        // the old one.
        reader.feed(&encode_text("list\t0"));
        assert_eq!(reader.next_frame(), Some(Ok(b"list\t0".to_vec())));
    }
}