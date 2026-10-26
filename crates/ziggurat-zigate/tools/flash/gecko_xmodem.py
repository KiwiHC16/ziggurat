#!/usr/bin/env python3
# Envoi d'un .gbl au bootloader Gecko (menu série 115200) en XMODEM-CRC, sans dépendance (Python 3.7).
# Usage : gecko_xmodem.py <tty> <fichier.gbl> [--dry-run]
import os, sys, time, termios, select

SOH, EOT, ACK, NAK, CAN, CRC_REQ = 0x01, 0x04, 0x06, 0x15, 0x18, 0x43

def open_tty(path):
    fd = os.open(path, os.O_RDWR | os.O_NOCTTY)
    a = termios.tcgetattr(fd)
    a[0] = 0                                   # iflag : ni IXON/IXOFF, ni conversions
    a[1] = 0                                   # oflag
    a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    a[3] = 0                                   # lflag : brut
    a[4] = a[5] = termios.B115200
    a[6][termios.VMIN] = 0
    a[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, a)
    termios.tcflush(fd, termios.TCIOFLUSH)
    return fd

def read_for(fd, seconds):
    out = bytearray()
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], max(0, end - time.time()))
        if r:
            out += os.read(fd, 4096)
    return bytes(out)

def read_until(fd, token, seconds):
    out = bytearray()
    end = time.time() + seconds
    while time.time() < end and token not in out:
        r, _, _ = select.select([fd], [], [], max(0, end - time.time()))
        if r:
            out += os.read(fd, 4096)
    return bytes(out)

def read_byte(fd, seconds):
    r, _, _ = select.select([fd], [], [], seconds)
    return os.read(fd, 1)[0] if r else None

def crc16(data):
    crc = 0
    for b in data:
        crc ^= b << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x1021) if crc & 0x8000 else (crc << 1)
            crc &= 0xFFFF
    return crc

def main():
    tty, gbl = sys.argv[1], sys.argv[2]
    dry = '--dry-run' in sys.argv
    data = open(gbl, 'rb').read()
    fd = open_tty(tty)

    # Menu
    read_for(fd, 0.5)
    os.write(fd, b'\n')
    menu = read_until(fd, b'BL >', 3)
    print('menu:', menu.decode(errors='replace').strip().replace('\r\n', ' | '))
    if b'upload gbl' not in menu:
        sys.exit('menu du bootloader absent')
    if dry:
        return

    read_for(fd, 0.3)
    os.write(fd, b'1')
    resp = read_until(fd, b'begin upload', 5)
    print('upload:', resp.decode(errors='replace').strip())
    if b'begin upload' not in resp:
        sys.exit('pas de "begin upload"')

    # Attente du 'C' du récepteur (XMODEM-CRC)
    end = time.time() + 15
    while time.time() < end:
        b = read_byte(fd, 1)
        if b == CRC_REQ:
            break
    else:
        sys.exit("pas de 'C' du récepteur")

    blocks = [data[i:i + 128] for i in range(0, len(data), 128)]
    for n, chunk in enumerate(blocks, 1):
        chunk = chunk.ljust(128, b'\xff')
        pkt = bytes([SOH, n & 0xFF, 0xFF - (n & 0xFF)]) + chunk + crc16(chunk).to_bytes(2, 'big')
        for attempt in range(10):
            os.write(fd, pkt)
            b = read_byte(fd, 5)
            while b == CRC_REQ:                # 'C' en retard : on l'ignore
                b = read_byte(fd, 5)
            if b == ACK:
                break
            if b == CAN:
                sys.exit('annulé par le bootloader au bloc %d' % n)
        else:
            sys.exit('échec au bloc %d (dernière réponse %r)' % (n, b))
        if n % 100 == 0 or n == len(blocks):
            print('bloc %d/%d' % (n, len(blocks)), flush=True)

    for attempt in range(10):
        os.write(fd, bytes([EOT]))
        if read_byte(fd, 5) == ACK:
            break
    else:
        sys.exit('EOT non acquitté')
    tail = read_until(fd, b'BL >', 10)
    print('fin:', tail.decode(errors='replace').strip().replace('\r\n', ' | '))
    if b'complete' not in tail.lower() and b'Serial upload complete' not in tail:
        sys.exit('pas de confirmation "upload complete"')

    # Lancer l'application
    os.write(fd, b'2')
    time.sleep(1)
    print('application lancée')

main()
