#!/bin/sh
# Установка selfsync одной командой: Debian, Ubuntu, Fedora с systemd.
#
#   curl -fsSL https://raw.githubusercontent.com/Darkildo/selfsync/main/deploy/install.sh \
#     | sudo sh -s -- --domain notes.example.com
#
# Ставит сервер из релиза server-X.Y.Z (сверяет SHA256SUMS), юниты systemd (сервер по
# сокету, таймеры verify, backup, sweep), Caddy с сертификатом Let's Encrypt для
# домена, обёртку selfsync-admin для служебных команд; печатает ссылку подключения с
# QR и в конце запускает selfsync doctor. Повторный запуск — обновление: данные
# архивируются, прежний бинарь остаётся рядом.
#
# Флаги:
#   --domain NAME     домен сервера (обязателен с Caddy; DNS уже указывает сюда)
#   --version X.Y.Z   версия (по умолчанию — последний релиз server-*)
#   --no-proxy        без Caddy: свой nginx/Caddy, сниппет для него — в выводе
#   --vault NAME      vault для первой ссылки (по умолчанию notes)
#   --binary PATH     готовый бинарь вместо скачивания (офлайн, тесты)
#   --units DIR       юниты из каталога вместо скачивания (по умолчанию — deploy/systemd
#                     рядом со скриптом, если он запущен из репозитория)
#   --no-start        только разложить файлы: без пакетов, systemctl и ссылки (CI)
#
# SELFSYNC_ROOT — корень для файлов (тесты); по умолчанию /.
set -eu

REPO=Darkildo/selfsync
ROOT=${SELFSYNC_ROOT:-}
VERSION=""
DOMAIN=""
PROXY=caddy
VAULT=notes
BINARY=""
UNITS=""
START=1

say() { printf '%s\n' "$*"; }
die() { printf 'ошибка: %s\n' "$*" >&2; exit 1; }

while [ $# -gt 0 ]; do
	case "$1" in
	--domain) DOMAIN=${2:?}; shift 2 ;;
	--version) VERSION=${2:?}; shift 2 ;;
	--no-proxy) PROXY=none; shift ;;
	--vault) VAULT=${2:?}; shift 2 ;;
	--binary) BINARY=${2:?}; shift 2 ;;
	--units) UNITS=${2:?}; shift 2 ;;
	--no-start) START=0; shift ;;
	-h | --help)
		[ -f "$0" ] && sed -n '2,/^set -eu/p' "$0" | sed '$d; s/^# \{0,1\}//'
		exit 0
		;;
	*) die "неизвестный флаг: $1 (см. --help)" ;;
	esac
done

if [ "$PROXY" = caddy ] && [ -z "$DOMAIN" ]; then
	die "нужен --domain notes.example.com (или --no-proxy для своего прокси)"
fi
case "$VAULT" in *[!a-z0-9-]* | "") die "имя vault'а — [a-z0-9-]" ;; esac
if [ -z "$ROOT" ] && [ "$(id -u)" != 0 ]; then
	die "нужны права root: sudo sh install.sh …"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() { # URL ФАЙЛ
	if command -v curl >/dev/null 2>&1; then
		curl -fsSL --retry 3 -o "$2" "$1"
	elif command -v wget >/dev/null 2>&1; then
		wget -q -O "$2" "$1"
	else
		die "нужен curl или wget"
	fi
}

# --- бинарь -------------------------------------------------------------------
if [ -n "$BINARY" ]; then
	[ -f "$BINARY" ] || die "нет файла $BINARY"
	cp "$BINARY" "$TMP/selfsync"
	[ -n "$VERSION" ] || VERSION=$("$BINARY" --version 2>/dev/null | sed 's/^[^ ]* //')
else
	case "$(uname -m)" in
	x86_64 | amd64) ARCH=x86_64 ;;
	aarch64 | arm64) ARCH=aarch64 ;;
	*) die "архитектура $(uname -m) не поддерживается (есть x86_64 и aarch64)" ;;
	esac
	if [ -z "$VERSION" ]; then
		fetch "https://api.github.com/repos/$REPO/releases?per_page=30" "$TMP/releases.json"
		VERSION=$(grep -o '"tag_name": *"server-[0-9][^"]*"' "$TMP/releases.json" |
			head -n 1 | sed 's/.*"server-\([^"]*\)"/\1/')
		[ -n "$VERSION" ] || die "не удалось узнать последнюю версию — укажите --version"
	fi
	f="selfsync-$ARCH-unknown-linux-musl"
	base="https://github.com/$REPO/releases/download/server-$VERSION"
	say "скачиваю selfsync $VERSION ($ARCH)…"
	fetch "$base/$f" "$TMP/$f"
	fetch "$base/SHA256SUMS" "$TMP/SHA256SUMS"
	(cd "$TMP" && grep "  $f\$" SHA256SUMS | sha256sum -c - >/dev/null) ||
		die "контрольная сумма $f не сошлась"
	mv "$TMP/$f" "$TMP/selfsync"
fi
chmod 0755 "$TMP/selfsync"
[ -n "$VERSION" ] || die "не удалось узнать версию бинаря — укажите --version"

# --- юниты --------------------------------------------------------------------
UNIT_FILES="selfsync.socket selfsync.service selfsync-sweep.service selfsync-sweep.timer
selfsync-verify.service selfsync-verify.timer selfsync-backup.service selfsync-backup.timer"
# Запуск из репозитория (sh deploy/install.sh) — юниты рядом; через `curl | sh` —
# с GitHub по тегу версии.
case "$0" in
*install.sh)
	here=$(dirname "$0")
	if [ -z "$UNITS" ] && [ -f "$here/systemd/selfsync.service" ]; then
		UNITS="$here/systemd"
	fi
	;;
esac
mkdir -p "$TMP/units"
for u in $UNIT_FILES; do
	if [ -n "$UNITS" ]; then
		cp "$UNITS/$u" "$TMP/units/$u"
	else
		fetch "https://raw.githubusercontent.com/$REPO/server-$VERSION/deploy/systemd/$u" "$TMP/units/$u"
	fi
done

# --- обновление: остановить и сохранить данные ----------------------------------
BIN="$ROOT/usr/local/bin/selfsync"
DATA="$ROOT/var/lib/private/selfsync"
OLD=""
if [ -x "$BIN" ]; then
	OLD=$("$BIN" --version 2>/dev/null | sed 's/^[^ ]* //' || true)
	say "обновление ${OLD:-?} → $VERSION"
	if [ "$START" = 1 ]; then
		systemctl stop selfsync.socket selfsync.service selfsync-sweep.timer \
			selfsync-verify.timer selfsync-backup.timer 2>/dev/null || true
	fi
	if [ -d "$DATA" ]; then
		archive="$ROOT/root/selfsync-data-${OLD:-old}-$(date +%Y%m%d-%H%M%S).tgz"
		mkdir -p "$ROOT/root"
		tar -czf "$archive" -C "$DATA" .
		say "данные сохранены в $archive"
	fi
	cp "$BIN" "$BIN-${OLD:-old}"
fi

mkdir -p "$ROOT/usr/local/bin" "$ROOT/etc/systemd/system/selfsync.service.d"
install -m 0755 "$TMP/selfsync" "$BIN"
for u in $UNIT_FILES; do
	install -m 0644 "$TMP/units/$u" "$ROOT/etc/systemd/system/$u"
done

URL=""
[ -n "$DOMAIN" ] && URL="https://$DOMAIN"
{
	echo "# Записано deploy/install.sh; правьте здесь, а не в selfsync.service."
	echo "[Service]"
	[ -n "$URL" ] && echo "Environment=SELFSYNC_PUBLIC_URL=$URL"
	[ "$PROXY" = caddy ] && echo "Environment=SELFSYNC_TRUSTED_PROXY=1"
	true
} >"$ROOT/etc/systemd/system/selfsync.service.d/install.conf"

# Служебные команды от имени сервиса (DynamicUser): права на данные остаются его.
# Вывод — через cat: systemd-run --pipe с терминалом на stdin/stdout падает в
# непривилегированном LXC.
cat >"$ROOT/usr/local/bin/selfsync-admin" <<EOF
#!/bin/sh
# selfsync от имени сервиса: selfsync-admin token list, selfsync-admin verify, …
st=\$(mktemp)
{ systemd-run --quiet --wait --collect --pipe \\
	-p DynamicUser=yes -p User=selfsync -p Group=selfsync \\
	-p StateDirectory=selfsync -p StateDirectoryMode=0700 -p UMask=0077 \\
	-p ReadWritePaths=-/var/lib/selfsync-backups \\
	-E SELFSYNC_PUBLIC_URL=$URL -E SELFSYNC_LOG=warn \\
	/usr/local/bin/selfsync "\$@" </dev/null 2>&1; echo \$? >"\$st"; } | cat
code=\$(cat "\$st"); rm -f "\$st"; exit "\$code"
EOF
chmod 0755 "$ROOT/usr/local/bin/selfsync-admin"

# --- Caddy ----------------------------------------------------------------------
caddy_site() {
	cat <<EOF
# selfsync — записано deploy/install.sh
$DOMAIN {
	request_body {
		max_size 17MB
	}

	@selfsync path /v1/* /join/*
	reverse_proxy @selfsync 127.0.0.1:8085 {
		transport http {
			read_timeout 60s
			write_timeout 60s
		}
		flush_interval -1
	}

	respond 404
}
EOF
}

if [ "$PROXY" = caddy ]; then
	if [ "$START" = 1 ] && ! command -v caddy >/dev/null 2>&1; then
		say "ставлю Caddy…"
		if command -v apt-get >/dev/null 2>&1; then
			apt-get update -q && DEBIAN_FRONTEND=noninteractive apt-get install -y -q caddy
		elif command -v dnf >/dev/null 2>&1; then
			dnf install -y -q caddy
		else
			die "не знаю, как поставить Caddy здесь — поставьте его сами или --no-proxy"
		fi
	fi
	conf="$ROOT/etc/caddy/Caddyfile"
	mkdir -p "$ROOT/etc/caddy"
	# Свой Caddyfile не трогаем: заменяется только пустой, наш прежний или
	# стандартный из пакета (раздаёт файлы, без reverse_proxy).
	ours() { grep -q 'selfsync — записано deploy/install.sh' "$conf"; }
	replaceable() {
		[ ! -s "$conf" ] && return 0
		ours && return 0
		grep -q 'file_server' "$conf" && ! grep -q 'reverse_proxy' "$conf"
	}
	if replaceable; then
		if [ -s "$conf" ] && ! ours; then
			cp "$conf" "$conf.orig"
		fi
		caddy_site >"$conf"
	else
		caddy_site >"$ROOT/etc/caddy/selfsync.caddy"
		say "Caddyfile уже настроен — добавьте в него строку: import selfsync.caddy"
	fi
else
	say "без прокси: selfsync слушает 127.0.0.1:8085; настройте свой (deploy/nginx, deploy/caddy)"
	say "и включите SELFSYNC_TRUSTED_PROXY=1 в /etc/systemd/system/selfsync.service.d/install.conf"
fi

if [ "$START" = 0 ]; then
	say "файлы разложены (--no-start): $BIN, юниты, selfsync-admin"
	exit 0
fi

# --- запуск ----------------------------------------------------------------------
systemctl daemon-reload
systemctl enable --now selfsync.socket selfsync-sweep.timer selfsync-verify.timer selfsync-backup.timer
if [ "$PROXY" = caddy ]; then
	systemctl enable caddy >/dev/null 2>&1 || true
	systemctl reload-or-restart caddy
fi

if [ -z "$OLD" ]; then
	say ""
	say "Первое устройство: откройте ссылку на нём (или отсканируйте QR телефоном)."
	if [ -n "$URL" ]; then
		/usr/local/bin/selfsync-admin link --vault "$VAULT" --name "first device" --url "$URL" || true
	else
		/usr/local/bin/selfsync-admin link --vault "$VAULT" --name "first device" \
			--url "https://notes.example.com" || true
		say "(адрес в ссылке — пример: замените на свой, когда настроите прокси)"
	fi
fi

say ""
say "Проверка установки (selfsync-admin doctor):"
/usr/local/bin/selfsync-admin doctor || true
say ""
say "selfsync $VERSION установлен. Служебные команды: selfsync-admin token list, selfsync-admin link …"
