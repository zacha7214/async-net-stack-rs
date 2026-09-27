//! Bounded classic DNS/IN/A codec. No EDNS, DNSSEC, or recursive resolution.
use super::ResolveError;
use std::net::Ipv4Addr;

pub fn normalize(input: &str) -> Result<String, ResolveError> {
    let name = input.strip_suffix('.').unwrap_or(input);
    if name.is_empty()
        || name.len() > 253
        || name.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    {
        return Err(ResolveError::InvalidName);
    }

    Ok(name.to_ascii_lowercase())
}

pub fn query(id: u16, name: &str) -> Vec<u8> {
    let mut out = vec![0; 12];
    out[..2].copy_from_slice(&id.to_be_bytes());
    out[2] = 1; // Recursion desired.
    out[5] = 1;

    for label in name.split('.') {
        out.push(label.len() as u8);
        out.extend_from_slice(label.as_bytes());
    }

    out.extend_from_slice(&[0, 0, 1, 0, 1]);
    out
}

fn word(b: &[u8], at: usize) -> Result<u16, ()> {
    Ok(u16::from_be_bytes(
        b.get(at..at + 2).ok_or(())?.try_into().map_err(|_| ())?,
    ))
}

fn long(b: &[u8], at: usize) -> Result<u32, ()> {
    Ok(u32::from_be_bytes(
        b.get(at..at + 4).ok_or(())?.try_into().map_err(|_| ())?,
    ))
}

fn name(b: &[u8], start: usize) -> Result<(String, usize), ()> {
    let mut at = start;
    let mut end = None;
    let mut out = String::new();

    // Bounds both compression loops and pathological label chains.
    for _ in 0..128 {
        let len = *b.get(at).ok_or(())?;
        if len & 0xc0 == 0xc0 {
            let target = (word(b, at)? & 0x3fff) as usize;
            if target < 12 || target >= at {
                return Err(());
            }
            end.get_or_insert(at + 2);
            at = target;
        } else if len == 0 {
            return Ok((out, end.unwrap_or(at + 1)));
        } else {
            if len > 63 {
                return Err(());
            }

            let label = b.get(at + 1..at + 1 + len as usize).ok_or(())?;
            if !out.is_empty() {
                out.push('.');
            }

            // Host names only. Reject binary labels rather than ambiguously decoding.
            if !label
                .iter()
                .all(|c| c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_')
            {
                return Err(());
            }

            out.push_str(std::str::from_utf8(label).map_err(|_| ())?);
            if out.len() > 253 {
                return Err(());
            }

            at += 1 + len as usize;
        }
    }

    Err(())
}

struct Record {
    owner: String,
    kind: u16,
    ttl: u32,
    start: usize,
    end: usize,
    authority: bool,
}

pub enum Answer {
    Addresses {
        canonical: String,
        addresses: Vec<Ipv4Addr>,
        ttl: u32,
    },
    Alias {
        target: String,
        ttl: u32,
        hops: usize,
    },
    Negative {
        error: ResolveError,
        ttl: Option<u32>,
    },
    Failure(ResolveError),
}

/// Err means malformed or mismatched: never turn it into a cache entry.
pub fn response(b: &[u8], id: u16, question: &str, alias_budget: usize) -> Result<Answer, ()> {
    if b.len() < 12 || b.len() > 512 || word(b, 0)? != id {
        return Err(());
    }

    let flags = word(b, 2)?;
    if flags & 0x8000 == 0 || flags & 0x7800 != 0 || flags & 0x0040 != 0 || word(b, 4)? != 1 {
        return Err(());
    }

    let (q, mut at) = name(b, 12)?;
    if !q.eq_ignore_ascii_case(question) || word(b, at)? != 1 || word(b, at + 2)? != 1 {
        return Err(());
    }

    at += 4;
    if flags & 0x0200 != 0 {
        return Ok(Answer::Failure(ResolveError::TcpRequired));
    }

    match flags & 15 {
        0 | 3 => {}
        2 => return Ok(Answer::Failure(ResolveError::ServerFailure)),
        5 => return Ok(Answer::Failure(ResolveError::Refused)),
        _ => return Ok(Answer::Failure(ResolveError::ProtocolError)),
    }

    let answers = word(b, 6)? as usize;
    let authority = word(b, 8)? as usize;
    let total = answers + authority + word(b, 10)? as usize;
    if total > 64 {
        return Err(());
    }

    let mut records = Vec::with_capacity(total);
    for i in 0..total {
        let (owner, next) = name(b, at)?;
        at = next;
        let kind = word(b, at)?;
        let class = word(b, at + 2)?;
        let ttl = long(b, at + 4)?;
        let len = word(b, at + 8)? as usize;

        at += 10;
        let end = at
            .checked_add(len)
            .filter(|end| *end <= b.len())
            .ok_or(())?;

        if class == 1 && i < answers + authority {
            records.push(Record {
                owner: owner.to_ascii_lowercase(),
                kind,
                ttl: if ttl & 0x8000_0000 != 0 { 0 } else { ttl },
                start: at,
                end,
                authority: i >= answers,
            });
        }

        at = end;
    }

    if at != b.len() {
        return Err(());
    }

    let mut current = question.to_owned();
    let mut seen = Vec::new();
    let mut ttl = u32::MAX;
    for _ in 0..=alias_budget.min(8) {
        if seen.contains(&current) {
            return Ok(Answer::Failure(ResolveError::AliasLimit));
        }

        seen.push(current.clone());
        let mut addresses = Vec::new();
        let mut alias = None;
        for rr in records
            .iter()
            .filter(|r| !r.authority && r.owner == current)
        {
            if rr.kind == 1 {
                if rr.end - rr.start != 4 {
                    return Err(());
                }
                let ip = Ipv4Addr::new(
                    b[rr.start],
                    b[rr.start + 1],
                    b[rr.start + 2],
                    b[rr.start + 3],
                );
                if !addresses.contains(&ip) {
                    addresses.push(ip);
                }
                ttl = ttl.min(rr.ttl);
            } else if rr.kind == 5 {
                let (target, end) = name(b, rr.start)?;
                if end != rr.end || target.is_empty() {
                    return Err(());
                }
                let target = target.to_ascii_lowercase();
                if alias.as_ref().is_some_and(|old| old != &target) {
                    return Err(());
                }
                alias = Some(target);
                ttl = ttl.min(rr.ttl);
            }
        }

        if !addresses.is_empty() {
            if alias.is_some() || flags & 15 == 3 {
                return Err(());
            }

            return Ok(Answer::Addresses {
                canonical: current,
                addresses,
                ttl,
            });
        }

        if let Some(target) = alias {
            current = target;
            continue;
        }

        let mut negative_ttl = None;
        for rr in records.iter().filter(|r| r.authority && r.kind == 6) {
            if !(rr.owner.is_empty()
                || current == rr.owner
                || current.ends_with(&format!(".{}", rr.owner)))
            {
                continue;
            }

            let (_, next) = name(b, rr.start)?;
            let (_, next) = name(b, next)?;
            if next + 20 != rr.end {
                return Err(());
            }

            let value = rr.ttl.min(long(b, next + 16)?).min(ttl);
            negative_ttl = Some(negative_ttl.map_or(value, |old: u32| old.min(value)));
        }

        if flags & 15 == 3 {
            return Ok(Answer::Negative {
                error: ResolveError::NotFound,
                ttl: negative_ttl,
            });
        }

        if current != question && negative_ttl.is_none() {
            return Ok(Answer::Alias {
                target: current,
                ttl,
                hops: seen.len() - 1,
            });
        }

        return Ok(Answer::Negative {
            error: ResolveError::NoData,
            ttl: negative_ttl,
        });
    }

    Ok(Answer::Failure(ResolveError::AliasLimit))
}
