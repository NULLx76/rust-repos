// Copyright (c) 2018 Pietro Albini <pietro@pietroalbini.org>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies
// of the Software, and to permit persons to whom the Software is furnished to do
// so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

use crate::config::Config;
use crate::prelude::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::{
    fs::{self, File, OpenOptions},
    io::{prelude::*, BufWriter},
};

#[derive(Default, Serialize, Deserialize)]
struct State {
    last_id: HashMap<String, usize>,
}

#[derive(Serialize, Deserialize)]
pub struct Repo {
    pub id: String,
    pub name: String,
    pub has_cargo_toml: bool,
    pub has_cargo_lock: bool,
}

pub struct Data {
    base_dir: PathBuf,

    csv_write_lock: Arc<Mutex<()>>,

    state_path: PathBuf,
    state_cache: Arc<Mutex<Option<State>>>,
}

impl Data {
    pub fn new(config: &Config) -> Self {
        Data {
            base_dir: config.data_dir.clone(),

            csv_write_lock: Arc::new(Mutex::new(())),

            state_path: config.data_dir.join("state.json"),
            state_cache: Arc::new(Mutex::new(None)),
        }
    }

    fn edit_state<T, F: Fn(&mut State) -> Fallible<T>>(&self, f: F) -> Fallible<T> {
        let mut state_cache = self.state_cache.lock().unwrap();

        if state_cache.is_none() {
            if self.state_path.exists() {
                *state_cache = Some(serde_json::from_slice(&fs::read(&self.state_path)?)?);
            } else {
                *state_cache = Some(Default::default());
            }
        }

        let state = state_cache.as_mut().unwrap();
        let result = f(state)?;

        let mut file = BufWriter::new(File::create(&self.state_path)?);
        serde_json::to_writer_pretty(&mut file, &state)?;
        file.write_all(&[b'\n'])?;

        Ok(result)
    }

    pub fn get_last_id(&self, platform: &str) -> Fallible<Option<usize>> {
        self.edit_state(|state| Ok(state.last_id.get(platform).cloned()))
    }

    pub fn set_last_id(&self, platform: &str, id: usize) -> Fallible<()> {
        self.edit_state(|state| {
            state.last_id.insert(platform.to_string(), id);
            Ok(())
        })
    }

    pub fn store_repo(&self, platform: &str, repo: Repo) -> Fallible<()> {
        // Ensure only one thread can write to CSV files at once
        let _lock = self.csv_write_lock.lock().unwrap();

        let file = self.base_dir.join(format!("{}.csv", platform));

        // Create the new file or append to it
        let mut csv = if file.exists() {
            csv::WriterBuilder::new()
                .has_headers(false)
                .from_writer(OpenOptions::new().append(true).open(&file)?)
        } else {
            csv::WriterBuilder::new().from_path(&file)?
        };

        csv.serialize(repo)?;

        Ok(())
    }
}
