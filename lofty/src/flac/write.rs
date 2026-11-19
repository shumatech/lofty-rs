use super::block::{BLOCK_ID_PADDING, BLOCK_ID_PICTURE, BLOCK_ID_VORBIS_COMMENTS, Block};
use super::read::verify_flac;
use crate::config::WriteOptions;
use crate::error::{FileEncodingError, FileParseError, SizeMismatchError, TagParseError};
use crate::id3::{FindId3v2Config, find_id3v2};
use crate::io::VerifiedFile;
use crate::macros::try_vec;
use crate::ogg::tag::VorbisCommentsRef;
use crate::picture::{Picture, PictureInformation};
use crate::tag::{Tag, TagType};
use crate::util::io::{FileLike, splice_tag};

use std::borrow::Cow;
use std::io::{Read, Seek};
use std::iter::Peekable;

use byteorder::{LittleEndian, ReadBytesExt};

pub(crate) fn write_to<F>(
	file: VerifiedFile<'_, F>,
	tag: &Tag,
	write_options: WriteOptions,
) -> Result<(), FileEncodingError>
where
	F: FileLike,
{
	match tag.tag_type() {
		TagType::VorbisComments => {
			let (vendor, items, pictures) = crate::ogg::tag::create_vorbis_comments_ref(tag);

			let mut comments_ref = VorbisCommentsRef {
				vendor: Cow::from(vendor),
				items,
				pictures,
			};

			write_to_inner(file, &mut comments_ref, write_options)
		},
		// This tag can *only* be removed in this format
		TagType::Id3v2 => {
			crate::id3::v2::tag::conversion::Id3v2TagRef::empty().write_to(file, write_options)
		},
		_ => unreachable!("tag type verified beforehand"),
	}
}

pub(crate) fn write_to_inner<'a, F, II, IP>(
	file: VerifiedFile<'_, F>,
	tag: &mut VorbisCommentsRef<'a, II, IP>,
	write_options: WriteOptions,
) -> Result<(), FileEncodingError>
where
	F: FileLike,
	II: Iterator<Item = (&'a str, &'a str)>,
	IP: Iterator<Item = (&'a Picture, PictureInformation)>,
{
	let mut file = file.into_inner();

	file.rewind()?;

	// We don't actually need the ID3v2 tag, but reading it will seek to the end of it if it exists
	find_id3v2(&mut file, FindId3v2Config::NO_READ_TAG).map_err(TagParseError::from)?;

	let mut stream_info = verify_flac(&mut file).map_err(FileParseError::from)?;

	let mut is_last_block = stream_info.last;
	let mut has_blocks_to_remove = false;

	stream_info.last = false; // Determined later

	let mut metadata_range = stream_info.start..stream_info.end;
	let mut blocks = vec![stream_info];
	while !is_last_block {
		let mut skip = false;
		let mut block = Block::read(&mut file, |ty| match ty {
			// Pictures are written again from the tag. The old padding is part of the space the
			// new blocks get, and is made again.
			BLOCK_ID_PICTURE | BLOCK_ID_PADDING => {
				has_blocks_to_remove = true;
				skip = true;
				false
			},
			_ => true,
		})
		.map_err(FileParseError::from)?;

		// Retain the original vendor string
		if block.ty == BLOCK_ID_VORBIS_COMMENTS {
			let reader = &mut &block.content[..];

			let vendor_len = reader.read_u32::<LittleEndian>()?;
			if vendor_len as usize > reader.len() {
				return Err(SizeMismatchError.into());
			}

			let mut vendor_raw = try_vec![0; vendor_len as usize]?;
			reader.read_exact(&mut vendor_raw)?;

			match String::from_utf8(vendor_raw) {
				Ok(vendor_str) => tag.vendor = Cow::Owned(vendor_str),
				// TODO: Error on strict?
				Err(_) => {
					log::warn!("FLAC vendor string is not valid UTF-8, not re-using");
					tag.vendor = Cow::Borrowed("");
				},
			}

			has_blocks_to_remove = true;
			skip = true;
		}

		is_last_block = block.last;
		metadata_range.end = block.end;

		if !skip {
			// Last block determined later
			block.last = false;
			blocks.push(block);
		}
	}

	let mut comments_peek = (&mut tag.items).peekable();
	let mut pictures_peek = (&mut tag.pictures).peekable();

	let has_comments = comments_peek.peek().is_some();
	let has_pictures = pictures_peek.peek().is_some();

	// Attempting to strip an already empty file
	if !has_blocks_to_remove && !has_comments && !has_pictures {
		log::debug!("Nothing to do");
		return Ok(());
	}

	let metadata_blocks = encode_tag(&tag.vendor, comments_peek, pictures_peek)?;

	blocks.extend(metadata_blocks);

	// The space the old metadata took, and what a `PADDING` block filling it would hold: `None`
	// inside when the new blocks fill it exactly, and no fit when they need more.
	let metadata_len = blocks
		.iter()
		.map(|block| u64::from(block.len()))
		.sum::<u64>();
	let old_metadata_len = metadata_range.end - metadata_range.start;
	let header_len = Block::BLOCK_HEADER_SIZE as u64;
	let fit = if metadata_len == old_metadata_len {
		Some(None)
	} else {
		old_metadata_len
			.checked_sub(metadata_len + header_len)
			.filter(|&left| left <= u64::from(Block::MAX_CONTENT_SIZE))
			.map(Some)
	};

	// Without shifting, a fit is taken however little padding it leaves, so nothing after the
	// metadata moves. Otherwise the preferred padding is the least there is.
	let padding_len = match (write_options.shift_data, fit) {
		(false, Some(padding_len)) => {
			log::trace!("Metadata fits, padding with {padding_len:?} bytes");
			padding_len
		},
		_ => write_options.preferred_padding.map(|preferred_padding| {
			u64::from(preferred_padding.get())
				.max(fit.flatten().unwrap_or(0))
				.min(u64::from(Block::MAX_CONTENT_SIZE))
		}),
	};

	if let Some(padding_len) = padding_len {
		let padding_len = usize::try_from(padding_len).map_err(|_| SizeMismatchError)?;

		// `PADDING` always goes last
		blocks.push(Block::new_padding(padding_len)?);
	}

	if let Some(block) = blocks.last_mut() {
		block.last = true;
	}

	let mut encoded_metadata = Vec::new();
	for block in blocks {
		block.write_to(&mut encoded_metadata)?;
		log::trace!(
			"Wrote a block (ty: {}, size: {})",
			block.ty,
			block.content.len()
		);
	}

	splice_tag(&mut file, metadata_range, &encoded_metadata, write_options)?;

	Ok(())
}

fn encode_tag<'a, II, IP>(
	vendor: &str,
	mut comments_peek: Peekable<&mut II>,
	pictures_peek: Peekable<&mut IP>,
) -> Result<Vec<Block>, FileEncodingError>
where
	II: Iterator<Item = (&'a str, &'a str)>,
	IP: Iterator<Item = (&'a Picture, PictureInformation)>,
{
	let mut metadata_blocks = Vec::new();

	if comments_peek.peek().is_some() {
		metadata_blocks.push(Block::new_comments(vendor, &mut comments_peek)?);
	}

	for (picture, info) in pictures_peek {
		metadata_blocks.push(Block::new_picture(picture, info)?);
	}

	Ok(metadata_blocks)
}
