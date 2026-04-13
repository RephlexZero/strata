use bytes::Bytes;
use std::net::UdpSocket;
use std::time::Duration;
use strata_bonding::protocol::header::BondingHeader;
use strata_bonding::receiver::transport::TransportBondingReceiver;
use strata_transport::wire::{Fragment, Packet as WirePacket, PacketHeader, PacketType, VarInt};

#[test]
fn test_transport_bonding_receiver_high_latency_gap() {
    let rcv = TransportBondingReceiver::new(Duration::from_millis(50));
    let rcv_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let rcv_addr = rcv_socket.local_addr().unwrap();
    rcv.add_link_socket(rcv_socket).unwrap();

    let send_socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    send_socket.connect(rcv_addr).unwrap();

    let send_pkt = |seq: u64, payload_str: &str| {
        let payload = Bytes::from(payload_str.to_string());
        // Bonding header (seq)
        let bh = BondingHeader::new(seq);
        let wrapped = bh.wrap(payload);

        // Transport header (seq)
        let th = PacketHeader {
            version: 1,
            packet_type: PacketType::Data,
            fragment: Fragment::Complete,
            is_keyframe: true,
            is_config: false,
            is_ppd_probe: false,
            payload_len: wrapped.len() as u16,
            sequence: VarInt::new(seq).unwrap(),
            timestamp_us: 0,
        };
        let wp = WirePacket {
            header: th,
            payload: wrapped,
        };
        send_socket.send(&wp.encode()).unwrap();
    };

    // Send pkt 0
    send_pkt(0, "pkt-0");
    match rcv.output_rx.recv_timeout(Duration::from_secs(1)) {
        Ok((data, _)) => assert_eq!(data, Bytes::from("pkt-0")),
        Err(_) => panic!("Failed to receive pkt-0"),
    }

    // Skip pkt 1, send pkt 2
    send_pkt(2, "pkt-2");

    // Nothing should be delivered yet, because the inner transport receiver
    // holds pkt 2, waiting for pkt 1 (up to 1000ms delay).
    let r = rcv.output_rx.recv_timeout(Duration::from_millis(400));
    assert!(r.is_err(), "Pkt 2 should not be gap-skipped within 400ms");

    // Now send pkt 1
    send_pkt(1, "pkt-1");

    // Both pkt 1 and pkt 2 should now emerge
    let mut received = Vec::new();
    for _ in 0..2 {
        match rcv.output_rx.recv_timeout(Duration::from_secs(1)) {
            Ok((data, _)) => received.push(data),
            Err(_) => panic!("Failed to receive missing packets"),
        }
    }

    assert_eq!(received[0], Bytes::from("pkt-1"));
    assert_eq!(received[1], Bytes::from("pkt-2"));
}
