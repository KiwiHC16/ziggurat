#!/bin/sh
# Wireshark sur l air Zigbee simulé de la VM Claude (ziggurat-sim, port 17760).
# Clés de déchiffrement : ~/.config/wireshark/zigbee_pc_keys
FIFO=/tmp/zigbee-sim.pcap
pkill -f "nc 192.168.4.124 17760" 2>/dev/null
rm -f "$FIFO"; mkfifo "$FIFO"
( while true; do nc 192.168.4.124 17760 > "$FIFO"; sleep 2; done ) &
LOOP=$!
/Applications/Wireshark.app/Contents/MacOS/Wireshark -k -i "$FIFO" -o "gui.window_title:Zigbee simulé (VM Claude)"
kill $LOOP 2>/dev/null; pkill -f "nc 192.168.4.124 17760" 2>/dev/null; rm -f "$FIFO"
