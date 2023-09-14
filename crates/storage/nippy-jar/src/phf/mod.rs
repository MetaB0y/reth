use crate::NippyJarError;
use serde::{
    de::Error as DeSerdeError, ser::Error as SerdeError, Deserialize, Deserializer, Serialize,
    Serializer,
};

pub trait KeySet {
    /// Add key to the list.
    fn add_keys(&mut self, keys: &[&[u8]]) -> Result<(), NippyJarError>;

    /// Get key index.
    fn get_index(&self, key: &[u8]) -> Result<Option<u64>, NippyJarError>;
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub enum Functions {
    Fmph(Fmph),
    // GoFmph(GoFmph),
    //Avoids irrefutable let errors. Remove this after adding another one.
    Unused,
}

#[derive(Default)]
pub struct Fmph {
    function: Option<ph::fmph::Function>,
}

impl Fmph {
    pub fn new() -> Self {
        Self { function: None }
    }
}

impl KeySet for Fmph {
    fn add_keys(&mut self, keys: &[&[u8]]) -> Result<(), NippyJarError> {
        self.function = Some(ph::fmph::Function::from(keys));
        Ok(())
    }

    fn get_index(&self, key: &[u8]) -> Result<Option<u64>, NippyJarError> {
        if let Some(f) = &self.function {
            return Ok(f.get(key))
        }
        Err(NippyJarError::PHFMissingKeys)
    }
}

impl PartialEq for Fmph {
    fn eq(&self, other: &Self) -> bool {
        match (&self.function, &other.function) {
            (Some(func1), Some(func2)) => {
                func1.level_sizes() == func2.level_sizes() &&
                    func1.write_bytes() == func2.write_bytes() &&
                    {
                        #[cfg(not(test))]
                        {
                            unimplemented!("No way to figure it out without exporting ( potentially expensive), so only allow direct comparison on a test")
                        }
                        #[cfg(test)]
                        {
                            let mut f1 = Vec::with_capacity(func1.write_bytes());
                            func1.write(&mut f1).expect("enough capacity");

                            let mut f2 = Vec::with_capacity(func2.write_bytes());
                            func2.write(&mut f2).expect("enough capacity");

                            return f1 == f2
                        }
                    }
            }
            (None, None) => true,
            _ => false,
        }
    }
}

impl std::fmt::Debug for Fmph {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fmph")
            .field("level_sizes", &self.function.as_ref().map(|f| f.level_sizes()))
            .field("bytes_size", &self.function.as_ref().map(|f| f.write_bytes()))
            .finish_non_exhaustive()
    }
}

impl Serialize for Fmph {
    /// Potentially expensive, but should be used only when creating the file.
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match &self.function {
            Some(f) => {
                let mut v = Vec::with_capacity(f.write_bytes());
                f.write(&mut v).map_err(S::Error::custom)?;
                serializer.serialize_bytes(&v)
            }
            None => serializer.serialize_bytes(&[]),
        }
    }
}

impl<'de> Deserialize<'de> for Fmph {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let buffer = <&[u8]>::deserialize(deserializer)?;

        if buffer.is_empty() {
            return Ok(Fmph { function: None })
        }

        Ok(Fmph {
            function: Some(
                ph::fmph::Function::read(&mut std::io::Cursor::new(buffer))
                    .map_err(D::Error::custom)?,
            ),
        })
    }
}
