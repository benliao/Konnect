use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcVector2 {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcFootprint {
    pub reference: String,
    pub value: String,
    pub footprint: String,
    pub position: IpcVector2,
    pub rotation: f64,
    pub layer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcTrack {
    /// KiCAD's KIID for this segment. `delete_trace` takes a uuid, so dropping
    /// it here made queried traces undeletable.
    pub uuid: String,
    pub net_name: String,
    pub layer: String,
    pub width: f64,
    pub start: IpcVector2,
    pub end: IpcVector2,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcNet {
    pub name: String,
    pub netcode: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcLayer {
    pub name: String,
    pub id: i32,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpcBoardExtents {
    pub min: IpcVector2,
    pub max: IpcVector2,
}
