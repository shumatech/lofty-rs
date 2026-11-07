use super::frame::read::ParsedFrame;
use super::header::Id3v2Header;
use super::tag::Id3v2Tag;
use crate::config::ParseOptions;
use crate::id3::v2::error::{FrameParseError, Id3v2ParseError};
use crate::id3::v2::util::synchsafe::UnsynchronizedStream;
use crate::id3::v2::{Frame, FrameId, FrameList, Id3v2TagFlags, Id3v2Version, TimestampFrame};
use crate::tag::items::Timestamp;

use std::borrow::Cow;
use std::io::{Read, Seek, SeekFrom};

// A reader of ID3v2 frames that can skip what it doesn't need, such as pictures when
// `ParseOptions::read_cover_art` is off. Skipping seeks when the stream allows it, so the skipped
// bytes are never read.
pub(crate) trait FrameRead: Read {
	fn skip(&mut self, count: u64) -> std::io::Result<()>;
}

// Skips by reading and discarding, for streams that can't seek.
pub(crate) struct ReadToSkip<R>(pub(crate) R);

impl<R: Read> Read for ReadToSkip<R> {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		self.0.read(buf)
	}
}

impl<R: Read> FrameRead for ReadToSkip<R> {
	fn skip(&mut self, count: u64) -> std::io::Result<()> {
		std::io::copy(&mut self.0.by_ref().take(count), &mut std::io::sink())?;
		Ok(())
	}
}

// Unsynchronization changes byte counts, so skipped bytes have to be read.
impl<R: Read> FrameRead for UnsynchronizedStream<R> {
	fn skip(&mut self, count: u64) -> std::io::Result<()> {
		std::io::copy(&mut self.by_ref().take(count), &mut std::io::sink())?;
		Ok(())
	}
}

// A window of a seekable stream: the next `len` bytes, or fewer at the end of the stream. Reads stop
// at the end of the window, and positions are relative to its start.
pub(crate) struct SeekWindow<'a, R> {
	reader: &'a mut R,
	start: u64,
	len: u64,
	pos: u64,
}

impl<'a, R: Read + Seek> SeekWindow<'a, R> {
	pub(crate) fn new(reader: &'a mut R, len: u64) -> std::io::Result<Self> {
		let start = reader.stream_position()?;
		let end = reader.seek(SeekFrom::End(0))?;
		reader.seek(SeekFrom::Start(start))?;
		Ok(Self {
			reader,
			start,
			len: len.min(end.saturating_sub(start)),
			pos: 0,
		})
	}

	// The bytes left in the window.
	pub(crate) fn remaining(&self) -> u64 {
		self.len.saturating_sub(self.pos)
	}
}

impl<R: Read + Seek> Read for SeekWindow<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let max = usize::try_from(self.remaining()).unwrap_or(usize::MAX);
		let read = max.min(buf.len());
		let read = self.reader.read(&mut buf[..read])?;
		self.pos += read as u64;
		Ok(read)
	}
}

impl<R: Read + Seek> Seek for SeekWindow<'_, R> {
	fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
		let target = match pos {
			SeekFrom::Start(offset) => Some(offset),
			SeekFrom::Current(offset) => self.pos.checked_add_signed(offset),
			SeekFrom::End(offset) => self.len.checked_add_signed(offset),
		};
		let Some(target) = target else {
			return Err(std::io::Error::new(
				std::io::ErrorKind::InvalidInput,
				"invalid seek to a negative or overflowing position",
			));
		};
		self.reader.seek(SeekFrom::Start(self.start + target))?;
		self.pos = target;
		Ok(target)
	}
}

impl<R: Read + Seek> FrameRead for SeekWindow<'_, R> {
	fn skip(&mut self, count: u64) -> std::io::Result<()> {
		let count = count.min(self.remaining());
		self.seek(SeekFrom::Current(count as i64))?;
		Ok(())
	}
}

pub(crate) fn parse_id3v2<R>(
	bytes: &mut R,
	header: Id3v2Header,
	parse_options: ParseOptions,
) -> Result<Id3v2Tag, Id3v2ParseError>
where
	R: Read + Seek,
{
	log::debug!(
		"Parsing ID3v2 tag, size: {}, version: {:?}",
		header.size,
		header.version
	);

	let mut tag_bytes = SeekWindow::new(bytes, u64::from(header.size - header.extended_size))?;

	// Prior to Id3v2.4, the tag-wide `unsynchronization` flag implied that unsynchronization was applied
	// to the *entire* content of the tag following the header, since there was no frame-level flag.
	// 2.4 added the `unsynchronization` flag to frame headers and made the tag-level flag equivalent to
	// setting flag on every frame.
	let mut ret;
	if header.flags.unsynchronisation && header.version < Id3v2Version::V4 {
		// Unsynchronize the entire tag
		let mut unsynchronized_reader = UnsynchronizedStream::new(tag_bytes);
		ret = read_all_frames_into_tag(&mut unsynchronized_reader, header, parse_options)?;

		// Get the `Take` back from the `UnsynchronizedStream`
		tag_bytes = unsynchronized_reader.into_inner();
	} else {
		ret = read_all_frames_into_tag(&mut tag_bytes, header, parse_options)?;
	}

	// Throw away the rest of the tag (padding, bad frames)
	let rest = tag_bytes.remaining();
	tag_bytes.skip(rest)?;

	// Construct TDRC frame from TYER, TDAT, and TIME frames
	if parse_options.implicit_conversions && header.version == Id3v2Version::V3 {
		construct_tdrc_from_v3(&mut ret);
	}

	Ok(ret)
}

fn construct_tdrc_from_v3(tag: &mut Id3v2Tag) {
	const TDRC: FrameId<'_> = FrameId::Valid(Cow::Borrowed("TDRC"));
	const TDAT: FrameId<'_> = FrameId::Valid(Cow::Borrowed("TDAT"));
	const TIME: FrameId<'_> = FrameId::Valid(Cow::Borrowed("TIME"));

	// Our TYER frame gets converted to TDRC earlier
	let Some(year_frame) = tag.remove(&TDRC).next() else {
		return;
	};

	let Frame::Timestamp(year_frame) = year_frame else {
		log::warn!("TYER frame is not a timestamp frame, retaining.");
		tag.insert(year_frame);
		return;
	};

	// This is not a TYER frame
	if year_frame.timestamp.month.is_some() {
		return;
	}

	let mut tdrc = Timestamp {
		year: year_frame.timestamp.year,
		..Timestamp::default()
	};

	let mut date_used = false;
	let mut time_used = false;
	'build: {
		let Some(date) = tag.get_text(&TDAT) else {
			break 'build;
		};

		if date.len() != 4 || !date.is_ascii() {
			log::warn!("Invalid TDAT frame, retaining.");
			break 'build;
		}

		let (Ok(day), Ok(month)) = (date[..2].parse::<u8>(), date[2..].parse::<u8>()) else {
			log::warn!("Invalid TDAT frame, retaining.");
			break 'build;
		};

		tdrc.month = Some(month);
		tdrc.day = Some(day);
		date_used = true;

		let Some(time) = tag.get_text(&TIME) else {
			break 'build;
		};

		if time.len() != 4 || !time.is_ascii() {
			log::warn!("Invalid TIME frame, retaining.");
			break 'build;
		}

		let (Ok(hour), Ok(minute)) = (time[..2].parse::<u8>(), time[2..].parse::<u8>()) else {
			log::warn!("Invalid TIME frame, retaining.");
			break 'build;
		};

		tdrc.hour = Some(hour);
		tdrc.minute = Some(minute);
		time_used = true;
	}

	tag.insert(Frame::Timestamp(TimestampFrame::new(
		FrameId::Valid(Cow::Borrowed("TDRC")),
		year_frame.encoding,
		tdrc,
	)));

	if date_used {
		let _ = tag.remove(&TDAT);
	}

	if time_used {
		let _ = tag.remove(&TIME);
	}
}

pub(super) fn read_all_frames_into_tag<R>(
	reader: &mut R,
	header: Id3v2Header,
	parse_options: ParseOptions,
) -> Result<Id3v2Tag, FrameParseError>
where
	R: FrameRead,
{
	let mut tag = Id3v2Tag::default();
	tag.original_version = header.version;
	tag.set_flags(header.flags);
	tag.frames = read_all_frames_into_list(reader, header.version, header.flags, parse_options)?;

	Ok(tag)
}

pub(crate) fn read_all_frames_into_list(
	reader: &mut dyn FrameRead,
	version: Id3v2Version,
	flags: Id3v2TagFlags,
	parse_options: ParseOptions,
) -> Result<FrameList<'static>, FrameParseError> {
	let mut list = FrameList::default();

	loop {
		match ParsedFrame::read(reader, version, flags, parse_options)? {
			ParsedFrame::Next(frame) => {
				let frame_value_is_empty = frame.is_empty();
				if let Some(replaced_frame) = list.insert(frame) {
					// Duplicate frames are not allowed. But if this occurs we try
					// to keep the frame with the non-empty content. Superfluous,
					// duplicate frames that follow the first frame are often empty.
					if frame_value_is_empty == Some(true)
						&& replaced_frame.is_empty() == Some(false)
					{
						log::warn!(
							"Restoring non-empty frame with ID \"{id}\" that has been replaced by \
							 an empty frame with the same ID",
							id = replaced_frame.id()
						);
						drop(list.insert(replaced_frame));
					} else {
						log::warn!(
							"Replaced frame with ID \"{id}\" by a frame with the same ID",
							id = replaced_frame.id()
						);
					}
				}
			},
			// No frame content found or ignored due to errors, but we can expect more frames
			ParsedFrame::Skip => {},
			// No frame content found, and we can expect there are no more frames
			ParsedFrame::Eof => break,
		}
	}

	Ok(list)
}

#[cfg(test)]
mod tests {
	use super::{FrameRead, SeekWindow, parse_id3v2};
	use crate::config::ParseOptions;
	use crate::tag::items::Timestamp;

	use std::io::{Cursor, Read, Seek, SeekFrom};

	#[test_log::test]
	fn seek_window_is_bounded() {
		let mut stream = Cursor::new((0..10).collect::<Vec<u8>>());
		stream.seek(SeekFrom::Start(2)).unwrap();

		let mut window = SeekWindow::new(&mut stream, 5).unwrap();
		let mut buf = [0; 3];
		window.read_exact(&mut buf).unwrap();
		assert_eq!(buf, [2, 3, 4]);
		assert_eq!(window.remaining(), 2);

		// Skipping stops at the end of the window
		window.skip(100).unwrap();
		assert_eq!(window.remaining(), 0);
		assert_eq!(window.read(&mut buf).unwrap(), 0);
		assert_eq!(stream.position(), 7);
	}

	#[test_log::test]
	fn seek_window_positions_are_relative() {
		let mut stream = Cursor::new((0..10).collect::<Vec<u8>>());
		stream.seek(SeekFrom::Start(4)).unwrap();

		// The window can't extend past the end of the stream
		let mut window = SeekWindow::new(&mut stream, 100).unwrap();
		assert_eq!(window.remaining(), 6);
		assert_eq!(window.seek(SeekFrom::End(-1)).unwrap(), 5);
		let mut buf = [0; 1];
		window.read_exact(&mut buf).unwrap();
		assert_eq!(buf, [9]);
		assert_eq!(window.stream_position().unwrap(), 6);
		assert_eq!(window.seek(SeekFrom::Start(1)).unwrap(), 1);
		window.read_exact(&mut buf).unwrap();
		assert_eq!(buf, [5]);
		assert!(window.seek(SeekFrom::Current(-10)).is_err());
	}

	#[test_log::test]
	fn zero_size_id3v2() {
		use crate::config::ParsingMode;
		use crate::id3::v2::header::Id3v2Header;

		use std::io::Cursor;

		let mut f = Cursor::new(std::fs::read("tests/tags/assets/id3v2/zero.id3v2").unwrap());
		let header = Id3v2Header::parse(&mut f).unwrap();
		assert!(
			parse_id3v2(
				&mut f,
				header,
				ParseOptions::new().parsing_mode(ParsingMode::Strict)
			)
			.is_ok()
		);
	}

	#[test_log::test]
	fn bad_frame_id_relaxed_id3v2() {
		use crate::config::ParsingMode;
		use crate::id3::v2::header::Id3v2Header;
		use crate::prelude::*;

		use std::io::Cursor;

		// Contains a frame with a "+" in the ID, which is invalid.
		// All other frames in the tag are valid, however.
		let mut f = Cursor::new(
			std::fs::read("tests/tags/assets/id3v2/bad_frame_otherwise_valid.id3v24").unwrap(),
		);
		let header = Id3v2Header::parse(&mut f).unwrap();
		let id3v2 = parse_id3v2(
			&mut f,
			header,
			ParseOptions::new().parsing_mode(ParsingMode::Relaxed),
		);
		assert!(id3v2.is_ok());

		let id3v2 = id3v2.unwrap();

		// There are 6 valid frames and 1 invalid frame
		assert_eq!(id3v2.len(), 6);

		assert_eq!(id3v2.title().as_deref(), Some("Foo title"));
		assert_eq!(id3v2.artist().as_deref(), Some("Bar artist"));
		assert_eq!(id3v2.comment().as_deref(), Some("Qux comment"));
		assert_eq!(
			id3v2.date(),
			Some(Timestamp {
				year: 1984,
				..Default::default()
			})
		);
		assert_eq!(id3v2.track(), Some(1));
		assert_eq!(id3v2.genre().as_deref(), Some("Classical"));
	}
}
