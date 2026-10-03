<?php
require '/var/www/html/core/php/core.inc.php';
foreach (['ab::zgIeeeAddr1','ab::zgIeeeAddrOk1'] as $k) echo "$k = ".json_encode(config::byKey($k,'Abeille'))."\n";
$eq = eqLogic::byLogicalId('Abeille1/0000','Abeille');
if ($eq) { foreach ($eq->getCmd('info') as $c) { $v = $c->execCmd(); if ($v !== '' && $v !== null) echo $c->getLogicalId()." = $v\n"; } }
else echo "pas d'équipement Abeille1/0000\n";
