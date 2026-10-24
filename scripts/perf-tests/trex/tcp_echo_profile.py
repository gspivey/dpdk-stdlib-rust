"""
TRex ASTF (Advanced Stateful) TCP echo profile for performance benchmarking.

Generates TCP connections: client sends a payload, server echoes it back,
then the connection is torn down. Measures throughput, latency, and CPS.

Usage (from TRex ASTF console):
    start -f tcp_echo_profile.py -t payload_size=64

Tunables:
    payload_size: Size of the request/response payload in bytes (default: 64)
    src_ip:       Client IP range start (default: 10.0.1.100)
    dst_ip:       Server IP (default: 10.0.1.200)
    dst_port:     Server TCP port (default: 9000)
"""

import os
import sys

sys.path.insert(0, '/opt/trex/automation/trex_control_plane/interactive')

from trex.astf.api import (
    ASTFProfile,
    ASTFIPGenDist,
    ASTFIPGen,
    ASTFGlobalInfo,
    ASTFGlobalInfoPerTemplate,
    ASTFTCPClientTemplate,
    ASTFTCPServerTemplate,
    ASTFProgram,
    ASTFTemplate,
    ASTFAssociation,
)


class Prof1:
    """TCP echo profile: connect, send payload, receive echo, close."""

    def __init__(self):
        pass

    def get_profile(self, tunables, **kwargs):
        payload_size = int(tunables.get('payload_size', 64))
        src_ip = tunables.get('src_ip', '10.0.1.100')
        dst_ip = tunables.get('dst_ip', '10.0.1.200')
        dst_port = int(tunables.get('dst_port', 9000))

        # Build request payload
        payload = b'T' * payload_size

        # Client program: connect, send payload, wait for echo, close
        prog_c = ASTFProgram()
        prog_c.send(payload)
        prog_c.recv(payload_size)

        # Server program: receive request, echo it back
        prog_s = ASTFProgram()
        prog_s.recv(payload_size)
        prog_s.send(payload)

        # IP generator: single client IP → single server IP
        ip_gen_c = ASTFIPGenDist(ip_range=src_ip, distribution="seq")
        ip_gen_s = ASTFIPGenDist(ip_range=dst_ip, distribution="seq")
        ip_gen = ASTFIPGen(
            glob=ASTFIPGenDist(ip_range="0.0.0.0", distribution="seq"),
            dist_client=ip_gen_c,
            dist_server=ip_gen_s,
        )

        # Template: client initiates TCP to server on dst_port
        template = ASTFTemplate(
            client_template=ASTFTCPClientTemplate(
                program=prog_c,
                port=dst_port,
            ),
            server_template=ASTFTCPServerTemplate(
                program=prog_s,
                assoc=ASTFAssociation(port=dst_port),
            ),
        )

        return ASTFProfile(
            default_ip_gen=ip_gen,
            templates=[template],
        )


def register():
    return Prof1()
