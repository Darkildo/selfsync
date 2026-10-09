# Caddy с плагином CGI: сборка
#   xcaddy build --with github.com/aksdb/caddy-cgi/v2
# selfsync запускается процессом на каждый запрос и ничего не держит в памяти.
# Каталог данных должен принадлежать пользователю, от которого работает Caddy
# (служебные команды запускать от него же: sudo -u caddy selfsync token add ...).
{
	order cgi before respond
}

notes.example.com {
	# Согласовано с SELFSYNC_MAX_BODY_SIZE (16 МиБ на метаданные и одну часть загрузки).
	request_body {
		max_size 17MB
	}

	@selfsync path /v1/* /join/*
	cgi @selfsync /usr/local/bin/selfsync {
		# Go-библиотека CGI передаёт заголовок Authorization как HTTP_AUTHORIZATION
		# (в отличие от Apache) — отдельная настройка не нужна.
		env SELFSYNC_DATA_DIR=/var/lib/selfsync SELFSYNC_PUBLIC_URL=https://notes.example.com SELFSYNC_LOG=warn
		# Отдача блобов потоком, без буферизации ответа целиком.
		unbuffered_output
	}

	respond 404
}
