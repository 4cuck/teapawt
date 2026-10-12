function playMedia(overlay, tagName) {
    const media = overlay.parentElement.querySelector(tagName);
    if (!media) return;
    const url = media.getAttribute("data-url");
    if (!url) return;
    const startTime = parseFloat(media.getAttribute("data-start") || "0");
    media.setAttribute("controls", "");
    overlay.style.display = "none";

    if (window.Hls && Hls.isSupported()) {
        const hls = new Hls({ autoStartLoad: false });
        hls.loadSource(url);
        hls.attachMedia(media);
        hls.on(Hls.Events.MANIFEST_PARSED, function () {
            hls.loadLevel = hls.levels.length - 1;
            hls.startLoad(startTime);
            media.play();
        });
    } else if (media.canPlayType("application/vnd.apple.mpegurl")) {
        media.src = url;
        media.addEventListener("canplay", function () {
            if (startTime > 0) media.currentTime = startTime;
            media.play();
        });
    }
}

function playVideo(overlay) { playMedia(overlay, "video"); }

document.addEventListener("click", function (event) {
    const overlay = event.target.closest(".hls-overlay");
    if (!overlay) return;
    event.preventDefault();
    playVideo(overlay);
});
