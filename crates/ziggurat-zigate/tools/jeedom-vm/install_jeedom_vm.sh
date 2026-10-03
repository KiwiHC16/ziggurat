#!/bin/sh
# Installation contrôlée de Jeedom (fork gitlab2 jeedom/core) sur la VM Claude.
# Reprend les étapes utiles de install/install.sh, sans dist-upgrade, sans fail2ban,
# sans option MariaDB supprimée (innodb_large_prefix).
#
# sudo : les "sudo <cmd>" lancés par www-data (Abeille : sudo socat, etc.) s'exécutent
# sous l'identité www-data elle-même, sans aucune élévation de privilèges. La VM porte
# les clés SSH de l'infrastructure : Jeedom n'y obtient pas les droits root.
#
# Usage (root) : sh /home/ben/dev/jeedomvm/install_jeedom_vm.sh
set -eu
# "su -c" garde le PATH de l'utilisateur, sans /usr/sbin (a2enconf, usermod, visudo)
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

SRC=/home/ben/dev/jeedomvm/core
WEB=/var/www/html

[ "$(id -u)" -eq 0 ] || { echo "À lancer en root"; exit 1; }
[ -f "$SRC/install/install.php" ] || { echo "Source absente : $SRC"; exit 1; }
if [ -e /root/jeedom_admin.txt ]; then
  echo "Jeedom déjà installé (voir /root/jeedom_admin.txt), arrêt."; exit 1
fi
# Reprise après une installation interrompue : code et base déjà en place
RESUME=no
[ -e "$WEB/core/config/common.config.php" ] && RESUME=yes

echo "== 1. Paquets"
apt-get update
apt-get install -y --no-install-recommends \
  apache2 apache2-utils ssl-cert mariadb-server mariadb-client \
  php libapache2-mod-php php-mysql php-curl php-gd php-xml php-opcache php-soap \
  php-common php-zip php-mbstring php-ldap \
  cron at sudo git unzip curl wget socat lsof ca-certificates \
  python3 python3-sysv-ipc python3-serial
# Optionnels : absents ou renommés selon les versions de Debian
for pkg in php-ssh2 php-imap; do
  apt-get install -y --no-install-recommends "$pkg" || echo "  ($pkg indisponible, ignoré)"
done

echo "== 2. MariaDB"
cat > /etc/mysql/conf.d/jeedom_my.cnf <<'EOF'
[mysqld]
skip-name-resolve
key_buffer_size = 16M
thread_cache_size = 16
tmp_table_size = 48M
max_heap_table_size = 48M
innodb_flush_method = O_DIRECT
innodb_flush_log_at_trx_commit = 2
EOF
mkdir -p /etc/systemd/system/mariadb.service.d
printf '[Service]\nRestart=always\nRestartSec=10\n' > /etc/systemd/system/mariadb.service.d/override.conf
systemctl daemon-reload
systemctl restart mariadb

echo "== 3. Code et base Jeedom"
if [ "$RESUME" = yes ]; then
  echo "  déjà en place, conservés"
else
mkdir -p "$WEB"
rm -f "$WEB/index.html"
cp -a "$SRC/." "$WEB/"

DBPASS=$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n')
mysql -uroot <<EOF
DROP USER IF EXISTS 'jeedom'@'localhost';
CREATE USER 'jeedom'@'localhost' IDENTIFIED BY '$DBPASS';
DROP DATABASE IF EXISTS jeedom;
CREATE DATABASE jeedom;
GRANT ALL PRIVILEGES ON jeedom.* TO 'jeedom'@'localhost';
FLUSH PRIVILEGES;
EOF
CFG="$WEB/core/config/common.config.php"
cp "$WEB/core/config/common.config.sample.php" "$CFG"
sed -i -e "s/#PASSWORD#/$DBPASS/g" -e "s/#DBNAME#/jeedom/g" -e "s/#USERNAME#/jeedom/g" \
       -e "s/#PORT#/3306/g" -e "s/#HOST#/localhost/g" "$CFG"
fi

echo "== 4. Apache et PHP"
cp "$WEB/install/apache_security" /etc/apache2/conf-available/security.conf
sed -i "s%WEBSERVER_HOME%$WEB%g" /etc/apache2/conf-available/security.conf
cp "$WEB/install/apache_default" /etc/apache2/sites-available/000-default.conf
sed -i "s%WEBSERVER_HOME%$WEB%g" /etc/apache2/sites-available/000-default.conf
a2enconf security >/dev/null
a2ensite 000-default >/dev/null
a2disconf other-vhosts-access-log >/dev/null 2>&1 || true
a2dismod -f status >/dev/null 2>&1 || true
a2enmod headers >/dev/null
# Jeedom partage /tmp/jeedom entre Apache et ses démons : pas de PrivateTmp
mkdir -p /etc/systemd/system/apache2.service.d
printf '[Service]\nPrivateTmp=no\nRestart=always\nRestartSec=10\n' > /etc/systemd/system/apache2.service.d/override.conf
for ini in $(find /etc/php -name php.ini -type f); do
  sed -i -e 's/max_execution_time = 30/max_execution_time = 600/' \
         -e 's/upload_max_filesize = 2M/upload_max_filesize = 1G/' \
         -e 's/post_max_size = 8M/post_max_size = 1G/' \
         -e 's/expose_php = On/expose_php = Off/' \
         -e 's/;\?opcache.enable=0/opcache.enable=1/' \
         -e 's/;\?opcache.enable_cli=0/opcache.enable_cli=1/' \
         -e 's/memory_limit = 128M/memory_limit = 256M/' "$ini"
done

echo "== 5. Répertoire temporaire, droits, groupes"
echo 'd /tmp/jeedom 0775 www-data www-data -' > /etc/tmpfiles.d/jeedom.conf
systemd-tmpfiles --create /etc/tmpfiles.d/jeedom.conf
usermod -a -G dialout,tty www-data
# Page "Sessions" de Jeedom : "sudo ls/cat" des sessions, exécuté en www-data (mode safe).
# Les fichiers appartiennent déjà à www-data (Apache) : aucun accès supplémentaire.
# php-common pose déjà un override (root root 1733) : on le remplace pour qu'il survive
# aux mises à jour du paquet
dpkg-statoverride --remove /var/lib/php/sessions 2>/dev/null || true
dpkg-statoverride --add --update root www-data 1770 /var/lib/php/sessions
chmod -R 775 "$WEB"
chown -R www-data:www-data "$WEB"
git config --system --add safe.directory "$WEB"

echo "== 6. sudo sans élévation pour www-data"
printf 'Defaults:www-data runas_default=www-data\nwww-data ALL=(www-data) NOPASSWD: ALL\n' > /etc/sudoers.d/jeedom
chmod 440 /etc/sudoers.d/jeedom
visudo -cf /etc/sudoers.d/jeedom

echo "== 7. Installation de la base Jeedom"
systemctl daemon-reload
systemctl restart apache2
sudo -u www-data php "$WEB/install/install.php" mode=force

echo "== 8. Crons"
echo "* * * * * www-data /usr/bin/php $WEB/core/php/jeeCron.php >> /dev/null" > /etc/cron.d/jeedom
echo "*/5 * * * * root /usr/bin/php $WEB/core/php/watchdog.php >> /dev/null" > /etc/cron.d/jeedom_watchdog
# cron ignore les fichiers de /etc/cron.d modifiables par le groupe (umask 002 hérité de su)
chmod 644 /etc/cron.d/jeedom /etc/cron.d/jeedom_watchdog

echo "== 9. Mot de passe admin aléatoire"
ADMINPASS=$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')
sudo -u www-data php -r "require '$WEB/core/php/core.inc.php'; \$u = user::byLogin('admin'); \$u->setPassword('$ADMINPASS'); \$u->save();"
umask 077
printf 'Jeedom VM Claude\nURL : http://192.168.4.124/\nadmin : %s\n' "$ADMINPASS" > /root/jeedom_admin.txt

echo "== 10. Vérification"
sudo -u www-data php "$WEB/sick.php" | tail -20 || true
echo "Terminé. Identifiants admin dans /root/jeedom_admin.txt"
