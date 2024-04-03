// Based on https://github.com/valeth/javelin/blob/master/javelin-codec/src/mpegts/transport_stream.rs with slight modification
use std::io::Write;

use mpeg2ts::ts::payload::Bytes;
use thiserror::Error;

use {
    bytes::Buf,
    mpeg2ts::{
        pes::PesHeader,
        time::{ClockReference, Timestamp},
        ts::{self, ContinuityCounter, Pid, TsHeader, TsPacket, TsPayload},
    },
    std::io::Cursor,
};

const PMT_PID: u16 = 256;
const VIDEO_ES_PID: u16 = 257;
const PES_VIDEO_STREAM_ID: u8 = 224;

#[derive(Error, Debug)]
pub enum TsError {
    #[error("Failed to create TS file")]
    FileCreationFailed(#[from] std::io::Error),

    #[error("Failed to write TS file")]
    WriteError,

    #[error("Packet ID {0} is not valid")]
    InvalidPacketId(u16),

    #[error("Invalid timestamp {0}")]
    InvalidTimestamp(u64),

    #[error("Packet payload exceeded packet limit")]
    PayloadTooBig,

    #[error("Clock reference value of {0} exceeds maximum")]
    ClockValueOutOfRange(u64),
    #[error("Mpeg2TsError: {0}")]
    Mpeg2TsError(#[from] mpeg2ts::Error),
}

pub struct TransportStream {
    video_continuity_counter: ContinuityCounter,
    packets: Vec<TsPacket>,
}

impl TransportStream {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn write_to<W: Write>(&mut self, wrt: W) -> Result<W, TsError> {
        use mpeg2ts::ts::{TsPacketWriter, WriteTsPacket};

        let mut writer = TsPacketWriter::new(wrt);
        writer
            .write_ts_packet(&default_pat_packet())
            .map_err(|_| TsError::WriteError)?;

        writer
            .write_ts_packet(&default_pmt_packet())
            .map_err(|_| TsError::WriteError)?;

        for packet in &self.packets {
            writer.write_ts_packet(packet)?;
        }

        Ok(writer.into_stream())
    }

    pub fn push_video(
        &mut self,
        timestamp: u64,
        composition_time: u64,
        keyframe: bool,
        video: Vec<u8>,
    ) -> Result<(), TsError> {
        use mpeg2ts::{
            es::StreamId,
            ts::{payload, AdaptationField},
        };

        let mut header = default_ts_header(VIDEO_ES_PID)?;
        header.continuity_counter = self.video_continuity_counter;

        let mut buf = Cursor::new(video.as_slice());
        let packet = {
            let data = {
                let pos = buf.position() as usize;
                let items = buf.remaining().min(153 - 1);
                let pes_data = if buf.remaining() < 153 {
                    &(buf.get_ref()[pos..pos + items])
                } else {
                    let pos = buf.position() as usize;
                    let items = buf.remaining().min(153 - 1);
                    &(buf.get_ref()[pos..pos + items])
                };
                make_raw_payload(pes_data)?
            };
            buf.advance(data.len());

            let pcr = make_clock_reference(timestamp * 90)?;

            let adaptation_field = if keyframe {
                Some(AdaptationField {
                    discontinuity_indicator: false,
                    random_access_indicator: true,
                    es_priority_indicator: false,
                    pcr: Some(pcr),
                    opcr: None,
                    splice_countdown: None,
                    transport_private_data: Vec::new(),
                    extension: None,
                })
            } else {
                None
            };

            let pts = make_timestamp((timestamp + composition_time) * 90)?;
            let dts = make_timestamp(timestamp * 90)?;

            let pes = payload::Pes {
                header: PesHeader {
                    stream_id: StreamId::new(PES_VIDEO_STREAM_ID),
                    priority: false,
                    data_alignment_indicator: false,
                    copyright: false,
                    original_or_copy: false,
                    pts: Some(pts),
                    dts: Some(dts),
                    escr: None,
                },
                pes_packet_len: 0,
                data,
            };
            TsPacket {
                header: header.clone(),
                adaptation_field,
                payload: Some(TsPayload::Pes(pes)),
            }
        };

        self.packets.push(packet);
        header.continuity_counter.increment();

        while buf.has_remaining() {
            let raw_payload = {
                let pos = buf.position() as usize;
                let items = buf.remaining().min(Bytes::MAX_SIZE - 1);

                let pes_data = if buf.remaining() < payload::Bytes::MAX_SIZE {
                    &(buf.get_ref()[pos..pos + items])
                } else {
                    &(buf.get_ref()[pos..pos + items])
                };
                make_raw_payload(pes_data)?
            };
            buf.advance(raw_payload.len());

            let packet = TsPacket {
                header: header.clone(),
                adaptation_field: None,
                payload: Some(TsPayload::Raw(raw_payload)),
            };

            self.packets.push(packet);
            header.continuity_counter.increment();
        }

        self.video_continuity_counter = header.continuity_counter;

        Ok(())
    }
}

impl Default for TransportStream {
    fn default() -> Self {
        Self {
            video_continuity_counter: ContinuityCounter::new(),
            packets: Vec::new(),
        }
    }
}

fn make_raw_payload(pes_data: &[u8]) -> Result<ts::payload::Bytes, TsError> {
    ts::payload::Bytes::new(pes_data).map_err(|_| TsError::PayloadTooBig)
}

fn make_timestamp(ts: u64) -> Result<Timestamp, TsError> {
    Timestamp::new(ts).map_err(|_| TsError::InvalidTimestamp(ts))
}

fn make_clock_reference(ts: u64) -> Result<ClockReference, TsError> {
    ClockReference::new(ts).map_err(|_| TsError::ClockValueOutOfRange(ts))
}

fn default_ts_header(pid: u16) -> Result<TsHeader, TsError> {
    use mpeg2ts::ts::TransportScramblingControl;

    Ok(TsHeader {
        transport_error_indicator: false,
        transport_priority: false,
        pid: Pid::new(pid).map_err(|_| TsError::InvalidPacketId(pid))?,
        transport_scrambling_control: TransportScramblingControl::NotScrambled,
        continuity_counter: ContinuityCounter::new(),
    })
}

fn default_pat_packet() -> TsPacket {
    use mpeg2ts::ts::{payload::Pat, ProgramAssociation, VersionNumber};

    TsPacket {
        header: default_ts_header(0).unwrap(),
        adaptation_field: None,
        payload: Some(TsPayload::Pat(Pat {
            transport_stream_id: 1,
            version_number: VersionNumber::default(),
            table: vec![ProgramAssociation {
                program_num: 1,
                program_map_pid: Pid::new(PMT_PID).unwrap(),
            }],
        })),
    }
}

fn default_pmt_packet() -> TsPacket {
    use mpeg2ts::{
        es::StreamType,
        ts::{payload::Pmt, EsInfo, VersionNumber},
    };

    TsPacket {
        header: default_ts_header(PMT_PID).unwrap(),
        adaptation_field: None,
        payload: Some(TsPayload::Pmt(Pmt {
            program_num: 1,
            pcr_pid: Some(Pid::new(VIDEO_ES_PID).unwrap()),
            version_number: VersionNumber::default(),
            program_info: vec![],
            es_info: vec![EsInfo {
                stream_type: StreamType::H264,
                elementary_pid: Pid::new(VIDEO_ES_PID).unwrap(),
                descriptors: vec![],
            }],
        })),
    }
}
