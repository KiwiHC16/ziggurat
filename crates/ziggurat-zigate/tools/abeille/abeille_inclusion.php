<?php
require '/var/www/html/core/php/core.inc.php';
include_once '/var/www/html/plugins/Abeille/core/config/Abeille.config.php';
Abeille::msgToCmd(PRIO_NORM, 'CmdAbeille1/0000/zgSetPermitMode', 'mode=start');
echo "inclusion demandée\n";
