<?php
require '/var/www/html/core/php/core.inc.php';
$p = plugin::byId('Abeille');
if (!$p->isActive()) { $p->setIsEnable(1); echo "Abeille activé\n"; }
$cfg = [
  'ab::gtwType1' => 'zigate', 'ab::gtwSubType1' => 'WIFI', 'ab::gtwPort1' => '/tmp/zigateWifi1',
  'ab::gtwIpAddr1' => '127.0.0.1:20108', 'ab::gtwEnabled1' => 'Y', 'ab::gtwChan1' => 25,
  'ab::forceZigateHybridMode' => 'N',
];
foreach ($cfg as $k => $v) config::save($k, $v, 'Abeille');
for ($i = 2; $i <= 6; $i++) config::save('ab::gtwEnabled'.$i, 'N', 'Abeille');
echo "Dépendances : ".json_encode(Abeille::dependancy_info())."\n";
Abeille::deamon_start(true);
sleep(5);
echo "Démon : ".json_encode(Abeille::deamon_info())."\n";
