use crate::chunk::File;
use crate::grep::Match;
use anyhow::Result;
use std::fs;
use std::path::Path;

pub(crate) fn read_matches<S: AsRef<str>>(dir: &Path, input: S) -> Vec<Result<Match>> {
    let path = dir.join(format!("{}.in", input.as_ref()));
    let path = path.as_path();
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            line.ends_with('*').then(|| {
                Ok(Match {
                    path: path.into(),
                    line_number: idx as u64 + 1,
                })
            })
        })
        .collect::<Vec<Result<Match>>>()
}

pub(crate) fn read_all_matches<S: AsRef<str>>(dir: &Path, inputs: &[S]) -> Vec<Result<Match>> {
    inputs
        .iter()
        .map(|input| read_matches(dir, input).into_iter())
        .flatten()
        .collect()
}

pub(crate) fn read_expected_chunks<S: AsRef<str>>(dir: &Path, input: S) -> Option<File> {
    let input = input.as_ref();
    let outfile = dir.join(format!("{}.out", input));
    let (chunks, lnums) = fs::read_to_string(&outfile)
        .unwrap()
        .lines()
        .filter(|s| !s.is_empty())
        .map(|line| {
            let mut s = line.split(',');
            let range = s.next().unwrap();
            let mut rs = range.split(' ');
            let chunk_start: u64 = rs.next().unwrap().parse().unwrap();
            let chunk_end: u64 = rs.next().unwrap().parse().unwrap();
            let lines = s.next().unwrap();
            let lnums: Vec<u64> = lines.split(' ').map(|s| s.parse().unwrap()).collect();
            ((chunk_start, chunk_end), lnums)
        })
        .fold(
            (Vec::new(), Vec::new()),
            |(mut chunks, mut lnums), (chunk, mut match_lnums)| {
                chunks.push(chunk);
                lnums.append(&mut match_lnums);
                (chunks, lnums)
            },
        );
    if chunks.is_empty() || lnums.is_empty() {
        return None;
    }
    let infile = dir.join(format!("{}.in", input));
    let contents = fs::read(&infile).unwrap();
    Some(File::new(infile, lnums, chunks, contents))
}

pub(crate) fn read_all_expected_chunks<S: AsRef<str>>(dir: &Path, inputs: &[S]) -> Vec<File> {
    inputs
        .iter()
        .filter_map(|input| read_expected_chunks(dir, input))
        .collect()
}