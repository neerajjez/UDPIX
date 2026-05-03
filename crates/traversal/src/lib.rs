// udpix-traversal — Phase 4 NAT traversal engine (STUN/TURN/ICE + UDP hole punching).
// Modules: stun, turn, ice, holepunch — implemented in a future sprint.
//
// Overview:
//   Enterprise clients behind NAT use UDP hole punching for direct P2P connections.
//   Symmetric NATs fall back to a TURN relay.  ICE automates candidate selection.
