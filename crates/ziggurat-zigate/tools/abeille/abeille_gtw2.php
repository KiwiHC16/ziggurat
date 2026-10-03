<?php
require '/var/www/html/core/php/core.inc.php';
$cfg = ['ab::gtwType2'=>'zigate','ab::gtwSubType2'=>'WIFI','ab::gtwPort2'=>'/tmp/zigateWifi2','ab::gtwIpAddr2'=>'127.0.0.1:20109','ab::gtwEnabled2'=>'Y','ab::gtwChan2'=>25,'ab::zgIeeeAddr2'=>'','ab::zgIeeeAddrOk2'=>0];
foreach ($cfg as $k=>$v) config::save($k, $v, 'Abeille');
Abeille::deamon_start(true);
sleep(20);
echo "zgIeeeAddr2 = ".json_encode(config::byKey('ab::zgIeeeAddr2','Abeille'))."\n";
$eq = eqLogic::byLogicalId('Abeille2/0000','Abeille');
if ($eq) foreach ($eq->getCmd('info') as $c) if (in_array($c->getLogicalId(), ['online','FW-Version','Network-Status','Network-Channel'])) echo $c->getLogicalId()." = ".$c->execCmd()."\n";
