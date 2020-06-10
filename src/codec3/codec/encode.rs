use super::{ConnectFlags, WILL_QOS_SHIFT};
use crate::codec3::packet::*;
use crate::codec3::*;
use bytes::{BufMut, Bytes, BytesMut};
use bytestring::ByteString;
use std::{convert::TryFrom, num::NonZeroU16};

pub fn get_encoded_size(packet: &Packet) -> usize {
    match *packet {
        Packet::Connect ( ref connect ) => {
            let Connect {ref last_will, ref client_id, ref username, ref password, ..} = *connect;

            // Protocol Name + Protocol Level + Connect Flags + Keep Alive
            let mut n = 2 + 4 + 1 + 1 + 2;

            // Client Id
            n += 2 + client_id.len();

            // Will Topic + Will Message
            if let Some(LastWill { ref topic, ref message, .. }) = *last_will {
                n += 2 + topic.len() + 2 + message.len();
            }

            if let Some(ref s) = *username {
                n += 2 + s.len();
            }

            if let Some(ref s) = *password {
                n += 2 + s.len();
            }

            n
        }

        Packet::Publish( Publish{ qos, ref topic, ref payload, .. }) => {
            // Topic + Packet Id + Payload
            if qos == QoS::AtLeastOnce || qos == QoS::ExactlyOnce {
                4 + topic.len() + payload.len()
            } else {
                2 + topic.len() + payload.len()
            }
        }

        Packet::ConnectAck { .. } | // Flags + Return Code
        Packet::PublishAck { .. } | // Packet Id
        Packet::PublishReceived { .. } | // Packet Id
        Packet::PublishRelease { .. } | // Packet Id
        Packet::PublishComplete { .. } | // Packet Id
        Packet::UnsubscribeAck { .. } => 2, // Packet Id
        Packet::Subscribe { ref topic_filters, .. } => {
            2 + topic_filters.iter().fold(0, |acc, &(ref filter, _)| acc + 2 + filter.len() + 1)
        }

        Packet::SubscribeAck { ref status, .. } => 2 + status.len(),

        Packet::Unsubscribe { ref topic_filters, .. } => {
            2 + topic_filters.iter().fold(0, |acc, filter| acc + 2 + filter.len())
        }

        Packet::PingRequest | Packet::PingResponse | Packet::Disconnect => 0,
    }
}

pub fn encode(
    packet: &Packet,
    dst: &mut BytesMut,
    content_size: usize,
) -> Result<(), EncodeError> {
    match packet {
        Packet::Connect(connect) => {
            dst.put_u8(packet_type::CONNECT);
            write_variable_length(content_size, dst);
            encode_connect(connect, dst)?;
        }
        Packet::ConnectAck {
            session_present,
            return_code,
        } => {
            dst.put_u8(packet_type::CONNACK);
            write_variable_length(content_size, dst);
            let flags_byte = if *session_present { 0x01 } else { 0x00 };
            let code: u8 = From::from(*return_code);
            dst.put_slice(&[flags_byte, code]);
        }
        Packet::Publish(publish) => {
            dst.put_u8(
                packet_type::PUBLISH_START
                    | (u8::from(publish.qos) << 1)
                    | ((publish.dup as u8) << 3)
                    | (publish.retain as u8),
            );
            write_variable_length(content_size, dst);
            publish.topic.encode(dst)?;
            if publish.qos == QoS::AtMostOnce {
                if publish.packet_id.is_some() {
                    return Err(EncodeError::MalformedPacket); // packet id must not be set
                }
            } else {
                publish
                    .packet_id
                    .ok_or(EncodeError::PacketIdRequired)?
                    .encode(dst)?;
            }
            dst.put(publish.payload.as_ref());
        }

        Packet::PublishAck { packet_id } => {
            dst.put_u8(packet_type::PUBACK);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
        }
        Packet::PublishReceived { packet_id } => {
            dst.put_u8(packet_type::PUBREC);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
        }
        Packet::PublishRelease { packet_id } => {
            dst.put_u8(packet_type::PUBREL);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
        }
        Packet::PublishComplete { packet_id } => {
            dst.put_u8(packet_type::PUBCOMP);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
        }
        Packet::Subscribe {
            packet_id,
            ref topic_filters,
        } => {
            dst.put_u8(packet_type::SUBSCRIBE);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
            for &(ref filter, qos) in topic_filters {
                filter.encode(dst)?;
                dst.put_u8(qos.into());
            }
        }
        Packet::SubscribeAck {
            packet_id,
            ref status,
        } => {
            dst.put_u8(packet_type::SUBACK);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
            let buf: Vec<u8> = status
                .iter()
                .map(|s| match *s {
                    SubscribeReturnCode::Success(qos) => qos.into(),
                    _ => 0x80u8,
                })
                .collect();
            dst.put_slice(&buf);
        }
        Packet::Unsubscribe {
            packet_id,
            ref topic_filters,
        } => {
            dst.put_u8(packet_type::UNSUBSCRIBE);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
            for filter in topic_filters {
                filter.encode(dst)?;
            }
        }
        Packet::UnsubscribeAck { packet_id } => {
            dst.put_u8(packet_type::UNSUBACK);
            write_variable_length(content_size, dst);
            packet_id.encode(dst)?;
        }
        Packet::PingRequest => dst.put_slice(&[packet_type::PINGREQ, 0]),
        Packet::PingResponse => dst.put_slice(&[packet_type::PINGRESP, 0]),
        Packet::Disconnect => dst.put_slice(&[packet_type::DISCONNECT, 0]),
    }

    Ok(())
}

fn encode_connect(connect: &Connect, dst: &mut BytesMut) -> Result<(), EncodeError> {
    let Connect {
        clean_session,
        keep_alive,
        ref last_will,
        ref client_id,
        ref username,
        ref password,
    } = *connect;

    b"MQTT".as_ref().encode(dst)?;

    let mut flags = ConnectFlags::empty();

    if username.is_some() {
        flags |= ConnectFlags::USERNAME;
    }
    if password.is_some() {
        flags |= ConnectFlags::PASSWORD;
    }

    if let Some(LastWill { qos, retain, .. }) = *last_will {
        flags |= ConnectFlags::WILL;

        if retain {
            flags |= ConnectFlags::WILL_RETAIN;
        }

        let b: u8 = qos as u8;

        flags |= ConnectFlags::from_bits_truncate(b << WILL_QOS_SHIFT);
    }

    if clean_session {
        flags |= ConnectFlags::CLEAN_SESSION;
    }

    dst.put_slice(&[MQTT_LEVEL, flags.bits()]);
    dst.put_u16(keep_alive);
    client_id.encode(dst)?;

    if let Some(LastWill {
        ref topic,
        ref message,
        ..
    }) = *last_will
    {
        topic.encode(dst)?;
        message.encode(dst)?;
    }

    if let Some(ref s) = *username {
        s.encode(dst)?;
    }

    if let Some(ref s) = *password {
        s.encode(dst)?;
    }
    Ok(())
}

trait Encode {
    fn encoded_size(&self) -> usize;

    fn encode(&self, buf: &mut BytesMut) -> Result<(), EncodeError>;
}

impl Encode for NonZeroU16 {
    fn encoded_size(&self) -> usize {
        2
    }
    fn encode(&self, buf: &mut BytesMut) -> Result<(), EncodeError> {
        buf.put_u16(self.get());
        Ok(())
    }
}

impl Encode for Bytes {
    fn encoded_size(&self) -> usize {
        2 + self.len()
    }
    fn encode(&self, buf: &mut BytesMut) -> Result<(), EncodeError> {
        let len = u16::try_from(self.len()).map_err(|_| EncodeError::InvalidLength)?;
        buf.put_u16(len);
        buf.extend_from_slice(self.as_ref());
        Ok(())
    }
}

impl Encode for ByteString {
    fn encoded_size(&self) -> usize {
        self.get_ref().encoded_size()
    }
    fn encode(&self, buf: &mut BytesMut) -> Result<(), EncodeError> {
        self.get_ref().encode(buf)
    }
}

impl<'a> Encode for &'a [u8] {
    fn encoded_size(&self) -> usize {
        2 + self.len()
    }
    fn encode(&self, buf: &mut BytesMut) -> Result<(), EncodeError> {
        let len = u16::try_from(self.len()).map_err(|_| EncodeError::InvalidLength)?;
        buf.put_u16(len);
        buf.extend_from_slice(self);
        Ok(())
    }
}

#[inline]
fn write_variable_length(size: usize, dst: &mut BytesMut) {
    // todo: verify at higher level
    // if size > MAX_VARIABLE_LENGTH {
    //     Err(Error::new(ErrorKind::Other, "out of range"))
    if size <= 127 {
        dst.put_u8(size as u8);
    } else if size <= 16383 {
        // 127 + 127 << 7
        dst.put_slice(&[((size % 128) | 0x80) as u8, (size >> 7) as u8]);
    } else if size <= 2_097_151 {
        // 127 + 127 << 7 + 127 << 14
        dst.put_slice(&[
            ((size % 128) | 0x80) as u8,
            (((size >> 7) % 128) | 0x80) as u8,
            (size >> 14) as u8,
        ]);
    } else {
        dst.put_slice(&[
            ((size % 128) | 0x80) as u8,
            (((size >> 7) % 128) | 0x80) as u8,
            (((size >> 14) % 128) | 0x80) as u8,
            (size >> 21) as u8,
        ]);
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use bytestring::ByteString;
    use std::num::NonZeroU16;

    use super::*;

    fn packet_id(v: u16) -> NonZeroU16 {
        NonZeroU16::new(v).unwrap()
    }

    #[test]
    fn test_encode_variable_length() {
        let mut v = BytesMut::new();

        write_variable_length(123, &mut v);
        assert_eq!(v, [123].as_ref());

        v.clear();

        write_variable_length(129, &mut v);
        assert_eq!(v, b"\x81\x01".as_ref());

        v.clear();

        write_variable_length(16383, &mut v);
        assert_eq!(v, b"\xff\x7f".as_ref());

        v.clear();

        write_variable_length(2097151, &mut v);
        assert_eq!(v, b"\xff\xff\x7f".as_ref());

        v.clear();

        write_variable_length(268435455, &mut v);
        assert_eq!(v, b"\xff\xff\xff\x7f".as_ref());

        // assert!(v.write_variable_length(MAX_VARIABLE_LENGTH + 1).is_err())
    }

    #[test]
    fn test_encode_fixed_header() {
        let mut v = BytesMut::new();
        let p = Packet::PingRequest;

        assert_eq!(get_encoded_size(&p), 0);
        encode(&p, &mut v, 0).unwrap();
        assert_eq!(v, b"\xc0\x00".as_ref());

        v.clear();

        let p = Packet::Publish(Publish {
            dup: true,
            retain: true,
            qos: QoS::ExactlyOnce,
            topic: ByteString::from_static("topic"),
            packet_id: Some(packet_id(0x4321)),
            payload: (0..255).collect::<Vec<u8>>().into(),
        });

        assert_eq!(get_encoded_size(&p), 264);
        encode(&p, &mut v, 264).unwrap();
        assert_eq!(&v[0..3], b"\x3d\x88\x02".as_ref());
    }

    fn assert_encode_packet(packet: &Packet, expected: &[u8]) {
        let mut v = BytesMut::with_capacity(1024);
        encode(packet, &mut v, get_encoded_size(packet)).unwrap();
        assert_eq!(expected.len(), v.len());
        assert_eq!(&expected[..], &v[..]);
    }

    #[test]
    fn test_encode_connect_packets() {
        assert_encode_packet(
            &Packet::Connect(Connect {
                clean_session: false,
                keep_alive: 60,
                client_id: ByteString::from_static("12345"),
                last_will: None,
                username: Some(ByteString::from_static("user")),
                password: Some(Bytes::from_static(b"pass")),
            }),
            &b"\x10\x1D\x00\x04MQTT\x04\xC0\x00\x3C\x00\
\x0512345\x00\x04user\x00\x04pass"[..],
        );

        assert_encode_packet(
            &Packet::Connect(Connect {
                clean_session: false,
                keep_alive: 60,
                client_id: ByteString::from_static("12345"),
                last_will: Some(LastWill {
                    qos: QoS::ExactlyOnce,
                    retain: false,
                    topic: ByteString::from_static("topic"),
                    message: Bytes::from_static(b"message"),
                }),
                username: None,
                password: None,
            }),
            &b"\x10\x21\x00\x04MQTT\x04\x14\x00\x3C\x00\
\x0512345\x00\x05topic\x00\x07message"[..],
        );

        assert_encode_packet(&Packet::Disconnect, b"\xe0\x00");
    }

    #[test]
    fn test_encode_publish_packets() {
        assert_encode_packet(
            &Packet::Publish(Publish {
                dup: true,
                retain: true,
                qos: QoS::ExactlyOnce,
                topic: ByteString::from_static("topic"),
                packet_id: Some(packet_id(0x4321)),
                payload: Bytes::from_static(b"data"),
            }),
            b"\x3d\x0D\x00\x05topic\x43\x21data",
        );

        assert_encode_packet(
            &Packet::Publish(Publish {
                dup: false,
                retain: false,
                qos: QoS::AtMostOnce,
                topic: ByteString::from_static("topic"),
                packet_id: None,
                payload: Bytes::from_static(b"data"),
            }),
            b"\x30\x0b\x00\x05topicdata",
        );
    }

    #[test]
    fn test_encode_subscribe_packets() {
        assert_encode_packet(
            &Packet::Subscribe {
                packet_id: packet_id(0x1234),
                topic_filters: vec![
                    (ByteString::from_static("test"), QoS::AtLeastOnce),
                    (ByteString::from_static("filter"), QoS::ExactlyOnce),
                ],
            },
            b"\x82\x12\x12\x34\x00\x04test\x01\x00\x06filter\x02",
        );

        assert_encode_packet(
            &Packet::SubscribeAck {
                packet_id: packet_id(0x1234),
                status: vec![
                    SubscribeReturnCode::Success(QoS::AtLeastOnce),
                    SubscribeReturnCode::Failure,
                    SubscribeReturnCode::Success(QoS::ExactlyOnce),
                ],
            },
            b"\x90\x05\x12\x34\x01\x80\x02",
        );

        assert_encode_packet(
            &Packet::Unsubscribe {
                packet_id: packet_id(0x1234),
                topic_filters: vec![
                    ByteString::from_static("test"),
                    ByteString::from_static("filter"),
                ],
            },
            b"\xa2\x10\x12\x34\x00\x04test\x00\x06filter",
        );

        assert_encode_packet(
            &Packet::UnsubscribeAck {
                packet_id: packet_id(0x4321),
            },
            b"\xb0\x02\x43\x21",
        );
    }

    #[test]
    fn test_encode_ping_packets() {
        assert_encode_packet(&Packet::PingRequest, b"\xc0\x00");
        assert_encode_packet(&Packet::PingResponse, b"\xd0\x00");
    }
}
