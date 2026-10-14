use crate::store::{receive_key, spend_key, StoredEvent};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use transparent_events::TransparentEvent;
pub(crate) mod event {
    use super::*;
    pub fn serialize<S: Serializer>(v: &TransparentEvent, s: S) -> Result<S::Ok, S::Error> {
        v.to_bytes().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<TransparentEvent, D::Error> {
        let b = Vec::<u8>::deserialize(d)?;
        TransparentEvent::from_bytes(&b).map_err(serde::de::Error::custom)
    }
}
pub(crate) mod events {
    use super::*;
    pub fn serialize<S: Serializer>(v: &[TransparentEvent], s: S) -> Result<S::Ok, S::Error> {
        v.iter()
            .map(|e| e.to_bytes())
            .collect::<Vec<_>>()
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<TransparentEvent>, D::Error> {
        Vec::<Vec<u8>>::deserialize(d)?
            .iter()
            .map(|b| TransparentEvent::from_bytes(b).map_err(serde::de::Error::custom))
            .collect()
    }
}
pub(crate) mod table {
    use super::*;
    use crate::client::Table;
    pub fn serialize<S: Serializer>(v: &Table, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Table::Directory => None,
            Table::Pages(i) => Some(*i),
        }
        .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Table, D::Error> {
        Ok(match Option::<u32>::deserialize(d)? {
            None => Table::Directory,
            Some(i) if i < 32 => Table::Pages(i),
            _ => return Err(serde::de::Error::custom("invalid table")),
        })
    }
}
pub(crate) mod receives {
    use super::*;
    use transparent_events::Txid;
    pub fn serialize<S: Serializer>(
        v: &BTreeMap<(Txid, u32), StoredEvent>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        v.values().collect::<Vec<_>>().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(Txid, u32), StoredEvent>, D::Error> {
        Vec::<StoredEvent>::deserialize(d)?
            .into_iter()
            .map(|e| match e.event {
                TransparentEvent::Receive(r) => Ok((receive_key(&r), e)),
                _ => Err(serde::de::Error::custom("invalid receive")),
            })
            .collect()
    }
}
pub(crate) mod spends {
    use super::*;
    use transparent_events::Txid;
    pub fn serialize<S: Serializer>(
        v: &BTreeMap<(Txid, u32, Txid, u32), StoredEvent>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        v.values().collect::<Vec<_>>().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<(Txid, u32, Txid, u32), StoredEvent>, D::Error> {
        Vec::<StoredEvent>::deserialize(d)?
            .into_iter()
            .map(|e| match e.event {
                TransparentEvent::Spend(r) => Ok((spend_key(&r), e)),
                _ => Err(serde::de::Error::custom("invalid spend")),
            })
            .collect()
    }
}
