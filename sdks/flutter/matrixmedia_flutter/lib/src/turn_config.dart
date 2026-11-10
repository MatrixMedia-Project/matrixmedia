/// Shared ICE server list for mm-switch WebRTC publishers and viewers.
///
/// Order matters: plain UDP TURN first (lowest latency where the path is
/// open), TCP fallback second. Some networks (VPN, corporate firewalls)
/// blackhole all UDP toward the media host — without the TCP entry ICE
/// stays `waiting` forever, no media flows, and recordings come out empty.
const turnIceServers = [
  {'urls': 'stun:stun.l.google.com:19302'},
  {
    'urls': 'turn:steegler.com:3478',
    'username': 'mm',
    'credential': 'mm_turn_secret_prod',
  },
  {
    'urls': 'turn:steegler.com:3478?transport=tcp',
    'username': 'mm',
    'credential': 'mm_turn_secret_prod',
  },
];
