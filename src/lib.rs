use std::{collections::BTreeMap, path::Path};

use defmt_elf2table::{Location, Table};

pub mod logger;

pub fn decode_loop(
    frames: &mut Vec<u8>,
    table: &Table,
    locs: &Option<BTreeMap<u64, Location>>,
    current_dir: &Path,
) -> anyhow::Result<()> {
    loop {
        match defmt_decoder::decode(&frames, &table) {
            Ok((frame, consumed)) => {
                // NOTE(`[]` indexing) all indices in `table` have already been
                // verified to exist in the `locs` map
                let loc = locs.as_ref().map(|locs| &locs[&frame.index()]);

                let (mut file, mut line, mut mod_path) = (None, None, None);
                if let Some(loc) = loc {
                    let relpath = if let Ok(relpath) = loc.file.strip_prefix(&current_dir) {
                        relpath
                    } else {
                        // not relative; use full path
                        &loc.file
                    };
                    file = Some(relpath.display().to_string());
                    line = Some(loc.line as u32);
                    mod_path = Some(loc.module.clone());
                }

                // Forward the defmt frame to our logger.
                logger::log_defmt(
                    &frame,
                    file.as_deref(),
                    line,
                    mod_path.as_ref().map(|s| &**s),
                );

                let num_frames = frames.len();
                frames.rotate_left(consumed);
                frames.truncate(num_frames - consumed);
            }
            Err(defmt_decoder::DecodeError::UnexpectedEof) => break,
            Err(defmt_decoder::DecodeError::Malformed) => {
                log::error!("failed to decode defmt data: {:x?}", frames);
                Err(defmt_decoder::DecodeError::Malformed)?
            }
        }
    }

    Ok(())
}
