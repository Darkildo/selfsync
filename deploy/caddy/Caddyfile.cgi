# Caddy с плагином CGI: сборка
#   xcaddy build --with github.com/aksdb/caddy-cgi/v2
# notesync запускается процессом на каждый запрос и ничего не держит в памяти.
# Каталог данных должен принадлежать пользователю, от которого работает Caddy
# (служебные команды запускать от него же: sudo -u caddy notesync token add ...).
{
	order cgi before respond
}

notes.example.com {
	# Согласовано с NOTESYNC_MAX_BODY_SIZE (16 МиБ на метаданные и одну часть загрузки).
	request_body {
		max_size 17MB
	}

	@notesync path /v1/* /join/*
	cgi @notesync /usr/local/bin/notesync {
		# Go-библиотека CGI передаёт заголовок Authorization как HTTP_AUTHORIZATION
		# (в отличие от Apache) — отдельная настройка не нужна.
		env NOTESYNC_DATA_DIR=/var/lib/notesync NOTESYNC_PUBLIC_URL=https://notes.example.com NOTESYNC_LOG=warn
		# Отдача блобов потоком, без буферизации ответа целиком.
		unbuffered_output
	}

	respond 404
}
