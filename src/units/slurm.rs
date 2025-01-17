//! Local Exceptions.

use std::{io, fs, thread};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::{Duration, SystemTime};
use arc_swap::ArcSwap;
use futures::future::{select, Either, FutureExt};
use rpki::slurm::{SlurmFile, ValidationOutputFilters};
use serde::Deserialize;
use crate::payload;
use crate::comms::{Gate, Link, Terminated, UnitStatus};
use crate::config::ConfigPath;
use crate::manager::Component;


//------------ Configuration -------------------------------------------------

/// How long should the update thread sleep before checking files again?
const UPDATE_SLEEP: Duration = Duration::from_secs(2);


//------------ LocalExceptions -----------------------------------------------

/// A unit applying local exceptions from files.
#[derive(Debug, Deserialize)]
pub struct LocalExceptions {
    /// The source to read data from.
    source: Link,

    /// A list of paths to the SLURM files.
    files: Vec<ConfigPath>,
}

impl LocalExceptions {
    pub async fn run(
        mut self, mut component: Component, mut gate: Gate
    ) -> Result<(), Terminated> {
        component.register_metrics(gate.metrics());
        let files = ExceptionSet::new(
            self.files.into_iter().map(Into::into).collect()
        );
        loop {
            let update = match select(
                self.source.query().boxed(), gate.process().boxed()
            ).await {
                Either::Left((Ok(update), _)) => update,
                Either::Left((Err(UnitStatus::Gone), _)) => return Ok(()),
                _ => continue
            };
            gate.update_data(files.apply(update)).await;
        }
    }
}


//------------ ExceptionSet -------------------------------------------------

/// A collection of all the local exception files we are using.
struct ExceptionSet {
    /// The content of the various files.
    ///
    /// This lives behind an `ArcSwap` so we can cheaply swap out the content
    /// if a file updates.
    files: Arc<Vec<ArcSwap<Content>>>,

    /// An alive check for the update thread.
    ///
    /// If the set gets dropped, so does the arc and the thread can check on
    /// it via a weak reference to it.
    alive: Arc<()>,
}

impl ExceptionSet {
    fn new(files: Vec<PathBuf>) -> Self {
        // Doing things in this order avoids the need for type annotations.
        let res = ExceptionSet {
            files: Arc::new(
                files.iter().map(|_| Default::default()).collect()
            ),
            alive: Arc::new(()),
        };
        let content = res.files.clone();
        let alive = Arc::downgrade(&res.alive);

        thread::spawn(move || {
            Self::update_thread(files, content, alive)
        });

        res
    }

    fn apply(&self, update: payload::Update) -> payload::Update {
        let serial = update.serial();
        let mut set = update.into_set();

        for file in self.files.iter() {
            set = file.load().apply(set);
            
        }

        payload::Update::new(serial, set, None)
    }

    fn update_thread(
        paths: Vec<PathBuf>,
        content: Arc<Vec<ArcSwap<Content>>>,
        alive: Weak<()>,
    ) {
        let mut modified = vec![None::<SystemTime>; paths.len()];

        loop {
            if alive.upgrade().is_none() {
                // The set has gone and so should we.
                return
            }

            for (path, (modified, content)) in
                paths.iter().zip(modified.iter_mut().zip(content.iter()))
            {
                // We simply ignore any errors for now.
                let _ = Self::update_file(path, modified, content);
            }

            thread::sleep(UPDATE_SLEEP);
        }
    }

    fn update_file(
        path: &Path,
        old_modified: &mut Option<SystemTime>,
        content: &ArcSwap<Content>
    ) -> Result<(), io::Error> {
        let new_modified = fs::metadata(path)?.modified()?;
        if let Some(old_modified) = old_modified.as_ref() {
            if new_modified >= *old_modified {
                return Ok(())
            }
        }

        content.store(Arc::new(
            SlurmFile::from_reader(
                io::BufReader::new(
                    fs::File::open(path)?
                )
            )?.into()
        ));
        *old_modified = Some(new_modified);
        Ok(())
    }
}


//------------ Content -------------------------------------------------------

/// The content of a SLURM file in slightly pre-processed form.
#[derive(Default)]
struct Content {
    filters: ValidationOutputFilters,
    assertions: payload::Pack,
}

impl Content {
    fn apply(&self, set: payload::Set) -> payload::Set {
        // First filters, then assertions.
        let filtered = set.filter(|payload| {
            !self.filters.drop_payload(payload)
        });
        let mut builder = filtered.to_builder();
        builder.insert_pack(self.assertions.clone());
        builder.finalize()
    }
}

impl From<SlurmFile> for Content {
    fn from(slurm: SlurmFile) -> Content {
        let mut assertions = payload::PackBuilder::empty();
        for payload in slurm.assertions.iter_payload() {
            assertions.insert_unchecked(payload)
        }
        let assertions = assertions.finalize();
        Content {
            filters: slurm.filters,
            assertions
        }
    }
}


//============ Tests =========================================================

#[cfg(test)]
mod test {
    use super::*;
    use crate::payload::testrig;
    use rpki::slurm::PrefixFilter;
    use rpki::rtr::payload::Payload;

    #[test]
    fn apply_content() {
        use rand::Rng;
        
        fn random_pack<T: Rng>(rng: &mut T, len: usize) -> payload::Pack {
            let mut res = payload::PackBuilder::empty();
            for _ in 0..len {
                res.insert_unchecked(testrig::p(rng.gen()))
            }
            res.finalize()
        }

        let mut rng = rand_pcg::Pcg32::new(
            0xcafef00dd15ea5e5, 0xa02bdbf7bb3c0a7
        );

        // First, let’s make a data set.
        let s1 = payload::Set::from(random_pack(&mut rng, 100));

        // Now, a pack that we first insert and then remove again via
        // local exceptions.
        let s2 = payload::Set::from(random_pack(&mut rng, 10));

        // And a pack which is what we are going to insert via local
        // exceptions.
        let p3 = random_pack(&mut rng, 15);

        // Now we can make the input and output sets.
        let input = s1.merge(&s2);
        let output = s1.merge(&payload::Set::from(p3.clone()));

        // Time to make the content.
        let content = Content {
            filters: ValidationOutputFilters {
                prefix: s2.iter().filter_map(|payload| {
                    match payload {
                        Payload::Origin(origin) => {
                            Some(PrefixFilter::new(
                                Some(origin.prefix.prefix()),
                                Some(origin.asn),
                                None
                            ))
                        }
                        _ => None
                    }
                }).collect(),
                bgpsec: Vec::new()
            },
            assertions: p3
        };

        assert_eq!(content.apply(input), output);
    }
}

