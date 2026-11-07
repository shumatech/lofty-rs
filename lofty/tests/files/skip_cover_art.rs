// With `ParseOptions::read_cover_art(false)`, pictures are seeked over rather than read, so reading
// tags from a slow or remote file doesn't transfer its cover art.

use lofty::config::{ParseOptions, WriteOptions};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::probe::Probe;
use lofty::tag::{Tag, TagType};

use std::io::{Cursor, Read, Seek, SeekFrom};

const PICTURE_SIZE: usize = 256 * 1024;

// Counts the bytes read through it, ignoring seeks.
struct CountingReader {
	inner: Cursor<Vec<u8>>,
	read: usize,
}

impl Read for CountingReader {
	fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
		let read = self.inner.read(buf)?;
		self.read += read;
		Ok(read)
	}
}

impl Seek for CountingReader {
	fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
		self.inner.seek(pos)
	}
}

// `asset` with a tag of `tag_type` holding an artist and a large picture.
fn with_large_picture(asset: &str, tag_type: TagType) -> Vec<u8> {
	let mut file = Cursor::new(std::fs::read(asset).unwrap());
	let mut tagged_file = Probe::new(&mut file)
		.guess_file_type()
		.unwrap()
		.read()
		.unwrap();

	let mut tag = Tag::new(tag_type);
	tag.set_artist(String::from("Foo artist"));
	tag.push_picture(
		Picture::unchecked(vec![0xAB; PICTURE_SIZE])
			.pic_type(PictureType::CoverFront)
			.mime_type(MimeType::Jpeg)
			.build(),
	);
	tagged_file.insert_tag(tag);

	file.rewind().unwrap();
	tagged_file
		.save_to(&mut file, WriteOptions::default())
		.unwrap();
	file.into_inner()
}

// Reads `bytes`, returning how many bytes were read, the artist and the picture count.
fn read_tags(bytes: Vec<u8>, tag_type: TagType, read_cover_art: bool) -> (usize, String, usize) {
	let mut reader = CountingReader {
		inner: Cursor::new(bytes),
		read: 0,
	};
	let tagged_file = Probe::new(&mut reader)
		.options(ParseOptions::new().read_cover_art(read_cover_art))
		.guess_file_type()
		.unwrap()
		.read()
		.unwrap();

	let tag = tagged_file.tag(tag_type).unwrap();
	let artist = tag.artist().unwrap().into_owned();
	let pictures = tag.picture_count() as usize;
	(reader.read, artist, pictures)
}

fn check(asset: &str, tag_type: TagType) {
	let bytes = with_large_picture(asset, tag_type);

	let (read, artist, pictures) = read_tags(bytes.clone(), tag_type, true);
	assert!(read > PICTURE_SIZE, "{asset}: the picture was read");
	assert_eq!(artist, "Foo artist");
	assert_eq!(pictures, 1);

	let (read, artist, pictures) = read_tags(bytes, tag_type, false);
	assert!(
		read < PICTURE_SIZE / 4,
		"{asset}: read {read} bytes, the picture is {PICTURE_SIZE}"
	);
	assert_eq!(artist, "Foo artist");
	assert_eq!(pictures, 0);
}

#[test_log::test]
fn mpeg_id3v2() {
	check("tests/files/assets/minimal/full_test.mp3", TagType::Id3v2);
}

#[test_log::test]
fn mp4_ilst() {
	check(
		"tests/files/assets/minimal/m4a_codec_aac.m4a",
		TagType::Mp4Ilst,
	);
}

#[test_log::test]
fn wav_id3v2() {
	check(
		"tests/files/assets/minimal/wav_format_pcm.wav",
		TagType::Id3v2,
	);
}

#[test_log::test]
fn aiff_id3v2() {
	check("tests/files/assets/minimal/full_test.aiff", TagType::Id3v2);
}

#[test_log::test]
fn flac_picture_block() {
	check(
		"tests/files/assets/minimal/full_test.flac",
		TagType::VorbisComments,
	);
}
