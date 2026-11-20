// With `WriteOptions::shift_data(false)`, a tag is written over the old one when it fits in its
// space and padding, and otherwise not at all: the error holds the splice it would have made, so
// the audio after the tag is never moved.

use crate::util::{named_temp_file, temp_file};

use lofty::config::WriteOptions;
use lofty::error::WouldShiftError;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{Tag, TagType};

use lofty::io::{Length, Truncate};
use std::fs::{File, OpenOptions};

use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::ops::Range;

fn no_shift() -> WriteOptions {
	WriteOptions::new().shift_data(false)
}

fn titled(tag_type: TagType, title: &str) -> Tag {
	let mut tag = Tag::new(tag_type);
	tag.set_title(title.to_owned());
	tag
}

fn contents(file: &mut File) -> Vec<u8> {
	file.rewind().unwrap();
	let mut bytes = Vec::new();
	file.read_to_end(&mut bytes).unwrap();
	file.rewind().unwrap();
	bytes
}

fn title<R: Read + Seek>(mut reader: R, tag_type: TagType) -> String {
	reader.rewind().unwrap();
	let tagged = Probe::new(reader)
		.guess_file_type()
		.unwrap()
		.read()
		.unwrap();
	tagged.tag(tag_type).unwrap().title().unwrap().into_owned()
}

/// Where the audio starts: after a FLAC file's metadata blocks, or after an MP3's ID3v2 tag
fn audio_start(bytes: &[u8], tag_type: TagType) -> usize {
	match tag_type {
		TagType::VorbisComments => {
			assert_eq!(&bytes[..4], b"fLaC");
			let mut pos = 4;
			loop {
				let last = bytes[pos] & 0x80 != 0;
				let len = u32::from_be_bytes([0, bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]]);
				pos += 4 + len as usize;
				if last {
					return pos;
				}
			}
		},
		TagType::Id3v2 => {
			assert_eq!(&bytes[..3], b"ID3");
			let size = bytes[6..10].iter().fold(0_usize, |size, &byte| {
				(size << 7) | usize::from(byte & 0x7F)
			});
			10 + size
		},
		_ => unreachable!(),
	}
}

/// The error's [`WouldShiftError`], from the chain of sources
fn would_shift<'a>(error: &'a (dyn core::error::Error + 'static)) -> Option<&'a WouldShiftError> {
	let mut source = Some(error);
	while let Some(error) = source {
		if let Some(shift) = error.downcast_ref::<WouldShiftError>() {
			return Some(shift);
		}
		source = error.source();
	}
	None
}

/// A copy of `asset` with a short title written as lofty writes by default, with its padding
fn padded(asset: &str, tag_type: TagType) -> File {
	let mut file = temp_file(asset);
	titled(tag_type, "Dogs")
		.save_to(&mut file, WriteOptions::new())
		.unwrap();
	file.rewind().unwrap();
	file
}

fn fits_in_place(asset: &str, tag_type: TagType) {
	let mut file = padded(asset, tag_type);
	let before = contents(&mut file);
	let start = audio_start(&before, tag_type);

	// A fit is taken however little padding it leaves, below the preferred padding too
	titled(tag_type, "Dogs (Live at Knebworth)")
		.save_to(&mut file, no_shift().preferred_padding(100_000))
		.unwrap();

	let after = contents(&mut file);
	assert_eq!(after.len(), before.len());
	assert_eq!(audio_start(&after, tag_type), start);
	assert!(after[start..] == before[start..], "the audio moved");
	assert_eq!(title(&mut file, tag_type), "Dogs (Live at Knebworth)");
}

fn too_big_is_refused(asset: &str, tag_type: TagType) {
	let mut file = padded(asset, tag_type);
	let before = contents(&mut file);
	let start = audio_start(&before, tag_type);
	let long = "Dogs ".repeat(4000);

	let error = titled(tag_type, &long)
		.save_to(&mut file, no_shift())
		.unwrap_err();
	let shift = would_shift(&error).expect("a WouldShiftError");
	assert!(contents(&mut file) == before, "the file changed");

	// The splice it would have made gives the file with the new tag and the same audio
	let range = shift.range();
	let mut spliced = before[..range.start as usize].to_vec();
	spliced.extend_from_slice(shift.replacement());
	spliced.extend_from_slice(&before[range.end as usize..]);
	assert!(
		spliced[audio_start(&spliced, tag_type)..] == before[start..],
		"the audio moved"
	);
	assert_eq!(title(Cursor::new(spliced), tag_type), long);
}

fn unchanged_writes_nothing(asset: &str, tag_type: TagType) {
	let file = named_temp_file(asset, None);
	titled(tag_type, "Dogs")
		.save_to_path(file.path(), WriteOptions::new())
		.unwrap();

	// Opened only for reading: any write would fail
	let mut read_only = OpenOptions::new().read(true).open(file.path()).unwrap();
	titled(tag_type, "Dogs")
		.save_to(&mut read_only, no_shift())
		.unwrap();
}

macro_rules! in_place_tests {
	($($format:ident => $asset:literal, $tag_type:ident;)*) => {
		$(
			mod $format {
				use super::*;

				#[test_log::test]
				fn a_tag_that_fits_is_written_in_place() {
					fits_in_place($asset, TagType::$tag_type);
				}

				#[test_log::test]
				fn a_tag_that_doesnt_fit_is_refused_with_its_splice() {
					too_big_is_refused($asset, TagType::$tag_type);
				}

				#[test_log::test]
				fn an_unchanged_tag_writes_nothing() {
					unchanged_writes_nothing($asset, TagType::$tag_type);
				}
			}
		)*
	};
}

in_place_tests! {
	flac => "tests/files/assets/minimal/full_test.flac", VorbisComments;
	mpeg => "tests/files/assets/minimal/full_test.mp3", Id3v2;
}

/// A file in memory that records the ranges read from it and written to it
#[derive(Default)]
struct Recording {
	inner: Cursor<Vec<u8>>,
	reads: Vec<Range<u64>>,
	writes: Vec<Range<u64>>,
}

impl Recording {
	fn new(contents: Vec<u8>) -> Self {
		Self {
			inner: Cursor::new(contents),
			..Self::default()
		}
	}

	fn data(&self) -> &[u8] {
		self.inner.get_ref()
	}

	/// Whether anything read or written touched `range`
	fn touched(&self, range: &Range<u64>) -> bool {
		let overlaps = |done: &Range<u64>| done.start < range.end && range.start < done.end;
		self.reads.iter().chain(&self.writes).any(overlaps)
	}
}

impl Read for Recording {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let start = self.inner.position();
		let read = self.inner.read(buf)?;
		self.reads.push(start..start + read as u64);
		Ok(read)
	}
}

impl Write for Recording {
	fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
		let start = self.inner.position();
		let written = self.inner.write(buf)?;
		self.writes.push(start..start + written as u64);
		Ok(written)
	}

	fn flush(&mut self) -> std::io::Result<()> {
		Ok(())
	}
}

impl Seek for Recording {
	fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
		self.inner.seek(pos)
	}
}

impl Truncate for Recording {
	fn truncate(&mut self, new_len: u64) -> std::io::Result<()> {
		Truncate::truncate(&mut self.inner, new_len)
	}
}

impl Length for Recording {
	fn len(&self) -> std::io::Result<u64> {
		Length::len(&self.inner)
	}
}

/// An atom's size and the size of its header
fn atom_header(bytes: &[u8], pos: usize) -> (usize, usize) {
	let size = u32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
	match size {
		1 => (
			u64::from_be_bytes(bytes[pos + 8..pos + 16].try_into().unwrap()) as usize,
			16,
		),
		0 => (bytes.len() - pos, 8),
		size => (size as usize, 8),
	}
}

/// The atoms in `range` of `bytes`: each one's fourcc, its whole range and where its data starts
fn atoms(bytes: &[u8], range: Range<usize>) -> Vec<([u8; 4], Range<usize>, usize)> {
	let mut atoms = Vec::new();
	let mut pos = range.start;
	while pos + 8 <= range.end {
		let (size, header) = atom_header(bytes, pos);
		let fourcc = bytes[pos + 4..pos + 8].try_into().unwrap();
		atoms.push((fourcc, pos..pos + size, pos + header));
		pos += size;
	}
	atoms
}

fn top_level(bytes: &[u8], fourcc: [u8; 4]) -> Range<u64> {
	let (_, range, _) = atoms(bytes, 0..bytes.len())
		.into_iter()
		.find(|(found, ..)| *found == fourcc)
		.unwrap();
	range.start as u64..range.end as u64
}

/// Where each chunk offset is in `moov`'s `stco` and `co64` atoms, and how wide it is
fn chunk_offset_fields(bytes: &[u8]) -> Vec<(usize, usize)> {
	fn walk(bytes: &[u8], range: Range<usize>, fields: &mut Vec<(usize, usize)>) {
		for (fourcc, atom, data) in atoms(bytes, range) {
			match &fourcc {
				b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" => {
					walk(bytes, data..atom.end, fields)
				},
				b"stco" | b"co64" => {
					let count = u32::from_be_bytes(bytes[data + 4..data + 8].try_into().unwrap());
					let width = if &fourcc == b"stco" { 4 } else { 8 };
					fields.extend((0..count as usize).map(|i| (data + 8 + i * width, width)));
				},
				_ => {},
			}
		}
	}

	let mut fields = Vec::new();
	walk(bytes, 0..bytes.len(), &mut fields);
	assert!(!fields.is_empty(), "no chunk offsets");
	fields
}

fn read_offset(bytes: &[u8], at: usize, width: usize) -> u64 {
	let mut offset = [0; 8];
	offset[8 - width..].copy_from_slice(&bytes[at..at + width]);
	u64::from_be_bytes(offset)
}

/// The chunk offsets in `moov`'s `stco` and `co64` atoms
fn chunk_offsets(bytes: &[u8]) -> Vec<u64> {
	chunk_offset_fields(bytes)
		.into_iter()
		.map(|(at, width)| read_offset(bytes, at, width))
		.collect()
}

/// `bytes` (`ftyp`, `moov`, `mdat`) with `moov` moved to the end, its chunk offsets with it
fn with_moov_last(bytes: &[u8]) -> Vec<u8> {
	let ftyp = top_level(bytes, *b"ftyp");
	let moov = top_level(bytes, *b"moov");
	let mdat = top_level(bytes, *b"mdat");
	assert!(ftyp.end == moov.start && moov.end == mdat.start && mdat.end == bytes.len() as u64);

	let mut moved = bytes[..moov.start as usize].to_vec();
	moved.extend_from_slice(&bytes[mdat.start as usize..]);
	let mut moov_bytes = bytes[moov.start as usize..moov.end as usize].to_vec();
	for (at, width) in chunk_offset_fields(&moov_bytes) {
		let offset = read_offset(&moov_bytes, at, width) - (moov.end - moov.start);
		moov_bytes[at..at + width].copy_from_slice(&offset.to_be_bytes()[8 - width..]);
	}
	moved.extend_from_slice(&moov_bytes);
	moved
}

/// Whether each chunk offset of `after` points at the bytes the same one did in `before`
fn same_chunks(before: &[u8], after: &[u8]) -> bool {
	let chunk = |bytes: &[u8], offset: u64| bytes[offset as usize..][..16].to_vec();
	let (old, new) = (chunk_offsets(before), chunk_offsets(after));
	old.len() == new.len()
		&& old
			.iter()
			.zip(&new)
			.all(|(&old, &new)| chunk(before, old) == chunk(after, new))
}

/// `asset` with a short title, written as lofty writes by default (so `ilst` has `free` beside it)
fn padded_mp4(asset: &str) -> Vec<u8> {
	let mut file = Recording::new(std::fs::read(asset).unwrap());
	titled(TagType::Mp4Ilst, "Dogs")
		.save_to(&mut file, WriteOptions::new())
		.unwrap();
	file.inner.into_inner()
}

/// `mdat`'s media data, which a tag write in place shouldn't read or write
fn media(bytes: &[u8]) -> Range<u64> {
	let mdat = top_level(bytes, *b"mdat");
	let (_, header) = atom_header(bytes, mdat.start as usize);
	mdat.start + header as u64..mdat.end
}

const MOOV_LAST: &str = "tests/files/assets/minimal/m4a_codec_aac.m4a";
const MOOV_FIRST: &str = "tests/files/assets/issue_746_extended_mdat.m4a";

#[test_log::test]
fn mp4_moov_last_fits_in_place() {
	let before = padded_mp4(MOOV_LAST);
	let mut file = Recording::new(before.clone());
	titled(TagType::Mp4Ilst, "Dogs (Live at Knebworth)")
		.save_to(&mut file, no_shift())
		.unwrap();

	assert_eq!(file.data().len(), before.len());
	assert!(
		!file.touched(&media(&before)),
		"the media data was read or written"
	);
	assert!(same_chunks(&before, file.data()));
	assert_eq!(
		title(Cursor::new(file.data()), TagType::Mp4Ilst),
		"Dogs (Live at Knebworth)"
	);
}

#[test_log::test]
fn mp4_moov_last_grows_at_the_end() {
	let before = padded_mp4(MOOV_LAST);
	let moov = top_level(&before, *b"moov");
	let long = "Dogs ".repeat(4000);
	let mut file = Recording::new(before.clone());
	titled(TagType::Mp4Ilst, &long)
		.save_to(&mut file, no_shift())
		.unwrap();

	assert!(file.data().len() > before.len());
	assert!(
		file.data()[..moov.start as usize] == before[..moov.start as usize],
		"something before moov changed"
	);
	assert!(
		!file.touched(&media(&before)),
		"the media data was read or written"
	);
	assert!(same_chunks(&before, file.data()));
	assert_eq!(title(Cursor::new(file.data()), TagType::Mp4Ilst), long);

	// It grew with padding, so the next edit fits
	let grown = file.data().len();
	file.rewind().unwrap();
	titled(TagType::Mp4Ilst, &format!("{long} (Live)"))
		.save_to(&mut file, no_shift())
		.unwrap();
	assert_eq!(file.data().len(), grown);
}

/// The size of the `free` atom after `ilst` in `moov.udta.meta`, if there's one
fn free_after_ilst(bytes: &[u8]) -> Option<usize> {
	let find = |range: Range<usize>, fourcc: &[u8; 4]| {
		atoms(bytes, range)
			.into_iter()
			.find(|(found, ..)| found == fourcc)
	};
	let (_, moov, moov_data) = find(0..bytes.len(), b"moov")?;
	let (_, udta, udta_data) = find(moov_data..moov.end, b"udta")?;
	let (_, meta, meta_data) = find(udta_data..udta.end, b"meta")?;
	// `meta` is a full atom: a version and flags first
	let children = atoms(bytes, meta_data + 4..meta.end);
	let ilst = children.iter().position(|(fourcc, ..)| fourcc == b"ilst")?;
	let (fourcc, free, _) = children.get(ilst + 1)?;
	(fourcc == b"free").then(|| free.len())
}

#[test_log::test]
fn mp4_a_tag_that_fills_its_space_exactly_is_written_in_place() {
	let before = padded_mp4(MOOV_FIRST);
	let free = free_after_ilst(&before).expect("padding after ilst");
	let mut file = Recording::new(before.clone());
	let exact = format!("Dogs{}", "!".repeat(free));
	titled(TagType::Mp4Ilst, &exact)
		.save_to(&mut file, no_shift())
		.unwrap();

	assert_eq!(file.data().len(), before.len());
	assert_eq!(free_after_ilst(file.data()), None);
	assert_eq!(title(Cursor::new(file.data()), TagType::Mp4Ilst), exact);
}

#[test_log::test]
fn mp4_moov_last_keeps_its_chunk_offsets() {
	// Chunks far into `mdat`, past where `moov` changes counted from its own start
	let before = with_moov_last(&padded_mp4(MOOV_FIRST));
	assert!(chunk_offsets(&before).len() > 1);
	let mut file = Recording::new(before.clone());
	titled(TagType::Mp4Ilst, &"Dogs ".repeat(4000))
		.save_to(&mut file, no_shift())
		.unwrap();

	assert!(file.data().len() > before.len());
	assert!(same_chunks(&before, file.data()), "the chunk offsets moved");
	assert_eq!(chunk_offsets(&before), chunk_offsets(file.data()));
}

#[test_log::test]
fn mp4_moov_first_fits_in_place() {
	let before = padded_mp4(MOOV_FIRST);
	let mdat = top_level(&before, *b"mdat");
	let mut file = Recording::new(before.clone());
	titled(TagType::Mp4Ilst, "Dogs (Live at Knebworth)")
		.save_to(&mut file, no_shift())
		.unwrap();

	assert_eq!(file.data().len(), before.len());
	assert!(
		file.data()[mdat.start as usize..] == before[mdat.start as usize..],
		"mdat changed"
	);
	assert!(
		!file.touched(&media(&before)),
		"the media data was read or written"
	);
	assert_eq!(
		title(Cursor::new(file.data()), TagType::Mp4Ilst),
		"Dogs (Live at Knebworth)"
	);
}

#[test_log::test]
fn mp4_moov_first_that_doesnt_fit_is_refused_with_moved_offsets() {
	let before = padded_mp4(MOOV_FIRST);
	let long = "Dogs ".repeat(4000);
	let mut file = Recording::new(before.clone());
	let error = titled(TagType::Mp4Ilst, &long)
		.save_to(&mut file, no_shift())
		.unwrap_err();
	let shift = would_shift(&error).expect("a WouldShiftError");
	assert!(file.data() == before, "the file changed");
	assert!(file.writes.is_empty());
	assert!(!file.touched(&media(&before)), "the media data was read");
	assert_eq!(shift.range(), top_level(&before, *b"moov"));

	let range = shift.range();
	let mut spliced = before[..range.start as usize].to_vec();
	spliced.extend_from_slice(shift.replacement());
	spliced.extend_from_slice(&before[range.end as usize..]);
	assert!(
		same_chunks(&before, &spliced),
		"the chunk offsets weren't moved"
	);
	assert_eq!(title(Cursor::new(spliced), TagType::Mp4Ilst), long);
}

#[test_log::test]
fn mp4_moov_first_shifts_by_default() {
	let before = padded_mp4(MOOV_FIRST);
	let long = "Dogs ".repeat(4000);
	let mut file = Recording::new(before.clone());
	titled(TagType::Mp4Ilst, &long)
		.save_to(&mut file, WriteOptions::new())
		.unwrap();

	assert!(file.data().len() > before.len());
	assert!(
		same_chunks(&before, file.data()),
		"the chunk offsets weren't moved"
	);
	assert_eq!(title(Cursor::new(file.data()), TagType::Mp4Ilst), long);
}
