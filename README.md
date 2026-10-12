# 1c-mcp

Менеджер [bsl-language-server](https://github.com/1c-syntax/bsl-language-server) и MCP-мост для проектов 1С:Предприятие 8.3. Один процесс сервера держит несколько конфигураций, у каждой свой Java-процесс. Агент в Cursor ходит в один MCP `1c`.

Версия первого релиза: **0.1.1**. Лицензия: [LGPL-3.0](LICENSE).

## На основе

Сборка трёх инструментов в один сервер, со своим устройством: несколько конфигураций в одном процессе, интерфейс, кэш ответов и один MCP для агента.

- [1c-lsp-mcp-skill](https://github.com/FSerg/1c-lsp-mcp-skill) — запуск нескольких bsl-language-server, веб-интерфейс и MCP.
- [mcp-bsl-lsp-bridge](https://github.com/SteelMorgan/mcp-bsl-lsp-bridge) — инструменты агента по коду конфигурации: диагностика, навигация, вызовы.
- [mcp-bsl-platform-context](https://github.com/alkoleft/mcp-bsl-platform-context) — справка по синтаксису и объектной модели платформы. Здесь она подключается JAR-файлом `mcp-bsl-context`.

## Что умеет

- Запускает отдельный bsl-language-server на каждый проект и показывает прогресс индексации.
- Отдаёт агенту диагностику, навигацию, граф вызовов, влияние символа, сложность и качество модуля.
- Подхватывает изменения `.bsl` / `.os` и сбрасывает кэш ответов до уведомления Java.
- Кэширует дорогие ответы LSP, пока проект в статусе `ready`.
- Режет тяжёлые списки страницами: `references`, `workspace_symbols`, `call_graph`.
- По умолчанию отдаёт агенту компактный формат TOON. Его можно выключить в настройках сервера: тогда все проекты получают обычный JSON.
- Даёт справку API платформы, если указаны `platform_path` и jar `mcp-bsl-context` из [mcp-bsl-platform-context](https://github.com/alkoleft/mcp-bsl-platform-context).
- Умеет работать как служба (Windows, systemd, launchd).

Интерфейс: http://127.0.0.1:4000  
MCP: http://127.0.0.1:9010/mcp

## Требования

- Java 17 или новее в `PATH`.
- JAR `bsl-language-server` (проверен с 1.0.7).
- Для справки платформы: установленная 1С и JAR `mcp-bsl-context`.
- Cursor или другой клиент MCP.
- Rust нужен только чтобы собрать сервер из исходников. На готовом portable-пакете его нет.

## Быстрый старт

### Готовый пакет Windows

В каталоге пакета:

1. `00-START.cmd` — сервер и браузер.
2. `01-CONNECT-PROJECT.cmd` — выбрать папку конфигурации и корень проекта. Скрипт пишет `<корень>/.cursor/mcp.json` и копирует skill `1c-mcp`.
3. В Cursor открыть корень проекта и перезагрузить MCP `1c`.
4. `02-STOP.cmd` — остановить сервер.

Собрать такой пакет из исходников:

```powershell
.\scripts\pack-portable.ps1 -Zip
```

JAR ищутся в `jars\` и в родительской папке. Явные пути: `-BslLsJar` и `-BslContextJar`.

### Из исходников

```powershell
cargo build --release -p lsp-skill-server -p lsp-skill-cli
.\target\release\lsp-skill-server.exe
```

Либо `.\install.ps1`, если JAR уже лежат рядом с репозиторием.

Дальше в интерфейсе:

1. Указать путь к JAR языка и, если нужна справка платформы, путь к платформе и к `mcp-bsl-context`.
2. Создать проект. `project_root_path` — папка, где работает агент. `root_path` — выгрузка конфигурации для bsl-language-server.
3. Нажать **Запустить** и дождаться статуса **Готов**. На больших конфигурациях индексация занимает несколько минут.
4. Скопировать ID проекта в заголовок `x-project-id`.

## Подключение Cursor

Файл `<корень проекта>/.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "1c": {
      "url": "http://127.0.0.1:9010/mcp",
      "headers": {
        "x-project-id": "<ID проекта из интерфейса>"
      }
    }
  }
}
```

Глобальный `%USERPROFILE%\.cursor\mcp.json` скрипт подключения не меняет.

CLI `lsp-skill` необязателен. ID проекта он берёт из `.env` (поиск вверх по папкам). Образец: [`.env.example`](.env.example).

```text
PROJECT_ID=<ID проекта>
```

Координаты в tools — с нуля. `file_path` — относительно `project_root_path`. Перед тяжёлым запросом нужен статус `ready`.

## Настройки сервера

Кнопка **Настройки сервера** в интерфейсе. Файл: `%APPDATA%\Infostart\LspSkill\config\config.toml` (на Linux и macOS — каталог `Infostart/LspSkill` из `directories`).

```toml
jar_path = "C:/path/to/bsl-language-server-1.0.7-exec.jar"
listen_host = "127.0.0.1"
http_port = 4000
log_level = "info"

mcp_enabled = true
mcp_port = 9010

platform_path = "C:/Program Files/1cv8/8.3.xx.xxxx"
bsl_context_jar_path = "C:/path/to/mcp-bsl-context-0.3.2.jar"

# true по умолчанию для всех проектов. false — обычный JSON.
use_toon_format = true
```

TOON и уровень лога общие для всех проектов. TOON применяется сразу. Новый уровень лога — после перезапуска сервера. События кэша (`hit`, `miss`, `eviction`) пишутся при `log_level = "debug"` в `server.log`.

Галочка **Debug LSP** на карточке проекта пишет протокол Java в лог этого проекта. К событиям кэша она не относится.

## Инструменты MCP

| Задача | Tools |
|---|---|
| Статус и диагностика | `status`, `diagnostics`, `code_actions` |
| Навигация | `symbols`, `definition`, `references`, `workspace_symbols` |
| Вызовы | `incoming_calls`, `outgoing_calls`, `call_graph`, `symbol_impact` |
| Качество | `complexity`, `quality_diagnostics`, `module_health` |
| Справка платформы | `search`, `info`, `getMembers`, `getMember`, `getConstructors` |

У `references`, `workspace_symbols` и плоского `call_graph` по умолчанию страница до 20 элементов (`detail=summary`), поле `next_offset` и группировка `by_file`. Следующая страница: `offset=next_offset`. Полный вид списка: `detail=full` (до 200). У `call_graph` значение `detail=full` возвращает дерево целиком.

## Состав репозитория

```text
crates/core     конфиг, SQLite, жизненный цикл Java, LSP, кэш, страницы
crates/server   HTTP, интерфейс, MCP
crates/cli      lsp-skill
portable/       00-START, 01-CONNECT, 02-STOP
scripts/        сборка portable-пакета
skills/1c-mcp   skill, который Connect копирует в проект
```

## Служба

```text
lsp-skill-server service install
lsp-skill-server service status
lsp-skill-server service uninstall
```

На Windows установка службы требует права администратора.

## Первый релиз 0.1.1

**1c-mcp 0.1.1** — первый публичный выпуск.

Сервер поднимает bsl-language-server на каждый проект 1С и отдаёт агенту один MCP `1c`: диагностика, навигация, граф вызовов, влияние символа, качество модуля и справка платформы. Интерфейс на порту 4000, MCP на порту 9010.

В этом выпуске:

- несколько проектов, прогресс индексации, свой JAR и свои аргументы JVM на проект;
- кэш ответов LSP в памяти процесса, сброс при изменении `.bsl` / `.os` и при перезапуске языка;
- страницы для тяжёлых списков и формат TOON по умолчанию для всех проектов;
- настройки сервера в интерфейсе: TOON и уровень лога;
- portable-пакет Windows: `00-START.cmd`, `01-CONNECT-PROJECT.cmd`, `02-STOP.cmd`;
- CLI `lsp-skill` для тех же базовых запросов по HTTP.

Нужны Java 17+ и JAR bsl-language-server. Rust на машине, где только запускают сервер, не нужен.

## Лицензия

[GNU LGPL v3.0](LICENSE). Текст GPL v3, на который ссылается LGPL, лежит в [COPYING](COPYING).
