<h1><img src="termset.svg" width="40" align="top" alt=""> termset</h1>

**Save your terminal layouts.**

- Store your terminal layout in a YAML file you can commit into git and open the same on your other devices
- Re-open the same Claude Code instances for different projects
- Work with your locally-developed software setups without long-lived Terminal instances

Termset is a terminal app, written in Rust. It uses [Alacritty](https://github.com/alacritty/alacritty), a fast cross-platform OpenGL terminal emulator under the hood.

## Usage

```
$ terms termset.yml     # open a layout file (created if missing)
$ terms                 # use ./termset.yml in the current directory
```

![termset running a full-stack workspace](demo/screenshot.png)

`termset.yml`:

```yaml
groups:
  - name: Frontend
    sessions:
      - name: web
        dir: ~/app/web
        command: npm run dev
      - name: storybook
        dir: ~/app/web
        command: npm run storybook
      - name: claude
        dir: ~/app/web
        command: claude
  - name: Backend
    sessions:
      - name: api
        dir: ~/app/api
        command: cargo watch -x run
      - name: worker
        dir: ~/app/api
        command: cargo run --bin worker
  - name: Infra
    sessions:
      - name: postgres
        dir: ~/app
        command: docker compose up db
      - name: redis
        dir: ~/app
        command: redis-server
```

## Shortcuts.

Special key is assumed as macOS (Cmd), Linux (Ctrl+Shift)

 - **New tab**. Ctrl+Shift+T
 - **Close tab**. Ctrl+Shift+W
 - **Navigate up/down tab**. Ctrl+Shift+Up/Down
 - **Collapse/expand section**. Ctrl+Shift+Left/Right
 - **Start / Stop session**. Ctrl+Shift+R / Ctrl+Shift+X
 - **Copy / Paste**. Ctrl+Shift+C / Ctrl+Shift+V (or right-click)
 - **Toggle sidebar**. Ctrl+Shift+B
 - **Edit layout**. Ctrl+Shift+, (opens the YAML in nano)
 - **Save current layout**. Ctrl+Shift+S
 - **Quit**. Ctrl+Shift+Q

