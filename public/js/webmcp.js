// Expose read-only teapawt actions to browsers that implement WebMCP.
// https://webmachinelearning.github.io/webmcp/
(function () {
    const ctx = document.modelContext || navigator.modelContext;
    if (!ctx || typeof ctx.registerTool !== "function") return;

    async function markdown(path) {
        const response = await fetch(path, { headers: { Accept: "text/markdown" } });
        const text = await response.text();
        if (!response.ok) throw new Error(text.slice(0, 300) || response.statusText);
        return text;
    }

    const tools = [
        {
            name: "search_posts",
            description: "Search public posts on this Twitter/X frontend.",
            inputSchema: {
                type: "object",
                properties: {
                    query: { type: "string", description: "Words, hashtags, or from:username." }
                },
                required: ["query"]
            },
            execute: (args) => markdown("/search?f=tweets&q=" + encodeURIComponent(args.query || ""))
        },
        {
            name: "get_profile",
            description: "Read a public profile and its recent posts.",
            inputSchema: {
                type: "object",
                properties: {
                    username: { type: "string", description: "Account name, without the @." }
                },
                required: ["username"]
            },
            execute: (args) => markdown("/" + encodeURIComponent(String(args.username || "").replace(/^@/, "")))
        },
        {
            name: "get_post",
            description: "Read one public post and the replies under it.",
            inputSchema: {
                type: "object",
                properties: {
                    username: { type: "string", description: "Author name, without the @." },
                    id: { type: "string", description: "Numeric post id." }
                },
                required: ["username", "id"]
            },
            execute: (args) => markdown(
                "/" + encodeURIComponent(String(args.username || "").replace(/^@/, ""))
                + "/status/" + encodeURIComponent(args.id || "")
            )
        }
    ];

    for (const tool of tools) {
        Promise.resolve(ctx.registerTool(tool)).catch(() => {});
    }
})();
