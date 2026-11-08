use crate::error::SoundFontError;
use crate::generator::Generator;
use crate::zone_info::ZoneInfo;

#[non_exhaustive]
pub(crate) struct Zone {
    pub(crate) generators: Vec<Generator>,
}

impl Zone {
    pub(crate) fn empty() -> Self {
        Self {
            generators: Vec::new(),
        }
    }

    fn new(info: &ZoneInfo, generators: &[Generator]) -> Result<Self, SoundFontError> {
        // Futureboard: the bag's generator indices come from the file. A run
        // that reaches past the generator list rejects the font instead of
        // panicking (a release build aborts on panic). A run whose next index
        // goes backwards stays an empty zone, as before.
        let count = info.generator_count.max(0) as usize;
        if count == 0 {
            return Ok(Self::empty());
        }
        let start = info.generator_index as usize;
        let segment = start
            .checked_add(count)
            .and_then(|end| generators.get(start..end))
            .ok_or(SoundFontError::InvalidZoneList)?;

        Ok(Self {
            generators: segment.to_vec(),
        })
    }

    pub(crate) fn create(
        infos: &[ZoneInfo],
        generators: &[Generator],
    ) -> Result<Vec<Zone>, SoundFontError> {
        if infos.len() <= 1 {
            return Err(SoundFontError::ZoneNotFound);
        }

        // The last one is the terminator.
        let count = infos.len() - 1;

        let mut zones: Vec<Zone> = Vec::new();
        for info in infos.iter().take(count) {
            zones.push(Zone::new(info, generators)?);
        }

        Ok(zones)
    }
}
