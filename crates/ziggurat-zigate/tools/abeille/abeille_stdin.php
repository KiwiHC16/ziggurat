<?php
// Reads "topic|payload" lines on stdin and queues them to Abeille's command daemon
require '/var/www/html/core/php/core.inc.php';
include_once '/var/www/html/plugins/Abeille/core/config/Abeille.config.php';
while (($line = fgets(STDIN)) !== false) {
  $line = trim($line); if ($line === '') continue;
  [$topic, $payload] = array_pad(explode('|', $line, 2), 2, '');
  Abeille::msgToCmd(PRIO_NORM, $topic, $payload);
  echo "envoyé $topic $payload\n"; flush();
}
