"""
TRex stateless traffic profile for UDP echo benchmarking.

Generates a single UDP stream with configurable packet size.
Includes a latency measurement stream (1 per 1000 packets).

Usage (from TRex console):
    start -f udp_echo_profile.py -m 1mpps --port 0
"""

from trex_stl_lib.api import *


class STLUdpEcho(object):
    """UDP echo traffic profile for performance testing."""

    def __init__(self):
        pass

    def create_stream(self, packet_size=64, src_ip="10.0.1.100", dst_ip="10.0.1.200",
                      src_port=12000, dst_port=9000, src_mac=None, dst_mac=None):
        """Create main traffic stream + latency measurement stream."""

        # Minimum Ethernet frame is 64 bytes (includes 4-byte FCS added by NIC)
        # So minimum on-wire payload area = 64 - 14(eth) - 20(ip) - 8(udp) - 4(fcs) = 18 bytes
        min_payload = max(0, packet_size - 14 - 20 - 8 - 4)
        payload = 'P' * min_payload

        # Build base packet
        base_pkt = Ether()
        if src_mac:
            base_pkt.src = src_mac
        if dst_mac:
            base_pkt.dst = dst_mac

        base_pkt = base_pkt / IP(src=src_ip, dst=dst_ip) / UDP(sport=src_port, dport=dst_port) / payload

        # Pad to desired size if needed
        pad = max(0, packet_size - len(base_pkt) - 4)  # -4 for FCS
        if pad > 0:
            base_pkt = base_pkt / Raw(b'\x00' * pad)

        # Main traffic stream — continuous, high rate
        main_stream = STLStream(
            packet=STLPktBuilder(pkt=base_pkt),
            mode=STLTXCont(),
            name='udp_main'
        )

        # Latency measurement stream — 1000 pps, tagged for RTT tracking
        latency_stream = STLStream(
            packet=STLPktBuilder(pkt=base_pkt),
            mode=STLTXCont(pps=1000),
            flow_stats=STLFlowLatencyStats(pg_id=0),
            name='udp_latency'
        )

        return [main_stream, latency_stream]

    def get_streams(self, tunables, **kwargs):
        """TRex entry point — called by TRex server to load the profile."""
        packet_size = tunables.get('packet_size', 64)
        src_ip = tunables.get('src_ip', '10.0.1.100')
        dst_ip = tunables.get('dst_ip', '10.0.1.200')
        src_port = int(tunables.get('src_port', 12000))
        dst_port = int(tunables.get('dst_port', 9000))
        src_mac = tunables.get('src_mac', None)
        dst_mac = tunables.get('dst_mac', None)

        return self.create_stream(
            packet_size=int(packet_size),
            src_ip=src_ip,
            dst_ip=dst_ip,
            src_port=src_port,
            dst_port=dst_port,
            src_mac=src_mac,
            dst_mac=dst_mac,
        )


# This is the profile object TRex expects
def register():
    return STLUdpEcho()
