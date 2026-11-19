// With `WriteOptions::shift_data(false)`, a tag is written over the old one when it fits in its
// space and padding, and otherwise not at all: the error holds the splice it would have made, so
// the audio after the tag is never moved.

use crate::util::{named_temp_file, temp_file};

use lofty::config::WriteOptions;
use lofty::error::WouldShiftError;
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{Tag, TagType};

use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read, Seek};

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
