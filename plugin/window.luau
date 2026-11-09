--!strict
-- Opens tern-kube in a new tab, or focuses the one already open. The browser
-- itself is the `tern-kube` program (a Surface Protocol app); this plugin only
-- launches it.

local TITLE = "Kubernetes"

local function open(cx: WindowCx)
	for _, pane in cx.session:panes() do
		if pane.title == TITLE then
			cx.layout:focus(pane.pane)
			return
		end
	end
	if cx.layout:new_tab({ command = "tern-kube" }) == nil then
		cx:toast("error", "Could not open tern-kube", "Install it with `cargo install --path .` in the tern-kube repo.")
	end
end

tern.command({
	id = "open",
	title = "Open Kubernetes",
	icon = "box",
	run = open,
})

tern.bind("alt+shift+cmd+k", "plugin.tern-kube.open")
