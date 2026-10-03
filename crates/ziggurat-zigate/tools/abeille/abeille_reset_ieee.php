<?php
require '/var/www/html/core/php/core.inc.php';
config::save('ab::zgIeeeAddr1', '', 'Abeille');
config::save('ab::zgIeeeAddrOk1', 0, 'Abeille');
Abeille::deamon_start(true);
sleep(15);
foreach (['ab::zgIeeeAddr1','ab::zgIeeeAddrOk1'] as $k) echo "$k = ".json_encode(config::byKey($k,'Abeille'))."\n";
$eq = eqLogic::byLogicalId('Abeille1/0000','Abeille');
foreach ($eq->getCmd('info') as $c) { $v = $c->execCmd(); if (in_array($c->getLogicalId(), ['online','FW-Version','Network-Status','Network-Channel'])) echo $c->getLogicalId()." = $v\n"; }
