#!/bin/sh
# Wireshark sur la clé MG24 réelle (sonde de ziggurat-zigate, port 17761).
# Clés de déchiffrement : ~/.config/wireshark/zigbee_pc_keys
FIFO=/tmp/zigbee-real.pcap
pkill -f "nc 192.168.4.124 17761" 2>/dev/null
rm -f "$FIFO"; mkfifo "$FIFO"
( while true; do nc 192.168.4.124 17761 > "$FIFO"; sleep 2; done ) &
LOOP=$!
/Applications/Wireshark.app/Contents/MacOS/Wireshark -k -i "$FIFO" -o "gui.window_title:Zigbee réel (clé MG24)"
kill $LOOP 2>/dev/null; pkill -f "nc 192.168.4.124 17761" 2>/dev/null; rm -f "$FIFO"
