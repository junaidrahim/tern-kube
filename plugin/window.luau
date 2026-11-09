--!strict
-- Opens tern-kube in a new tab, or focuses the one already open. The browser
-- itself is the `tern-kube` program (a Surface Protocol app); this plugin only
-- launches it.
--
-- It also brings the tab back after Tern restarts: the open pane's id is kept
-- in `tern.kv`; closing the pane (or quitting tern-kube) forgets it, so only a
-- tab that was open when Tern went away is reopened. tern-kube restores its
-- own contexts, kind and open object from its state file.

local TITLE = "Kubernetes"
local KEY = "pane"

local function find(cx: WindowCx): number?
	for _, pane in cx.session:panes() do
		if pane.title == TITLE then
			return pane.pane
		end
	end
	return nil
end

local function launch(cx: WindowCx): number?
	local pane = cx.layout:new_tab({ command = "tern-kube" })
	if pane == nil then
		cx:toast("error", "Could not open tern-kube", "Install it with `cargo install --path .` in the tern-kube repo.")
	else
		tern.kv.set(KEY, pane)
	end
	return pane
end

local function open(cx: WindowCx)
	local pane = find(cx)
	if pane ~= nil then
		cx.layout:focus(pane)
		tern.kv.set(KEY, pane)
		return
	end
	launch(cx)
end

tern.command({
	id = "open",
	title = "Open Kubernetes",
	icon = "box",
	run = open,
})

tern.bind("alt+shift+cmd+k", "plugin.tern-kube.open")

tern.on("pane_closed", function(ev: PaneEvent, _cx: WindowCx)
	if tern.kv.get(KEY) == ev.pane then
		tern.kv.set(KEY, nil)
	end
end)

tern.on("window_start", function(cx: WindowCx)
	if tern.kv.get(KEY) ~= nil and find(cx) == nil then
		launch(cx)
	end
end)
