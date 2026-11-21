(function () {
    "use strict";
    var el = document.getElementById("chart-payload");
    if (!el || typeof Chart === "undefined") {
        return;
    }

    var labels = JSON.parse(el.getAttribute("data-labels") || "[1]");
    var cumPnl = JSON.parse(el.getAttribute("data-pnl") || "[0]");
    var drawdown = JSON.parse(el.getAttribute("data-dd") || "[0]");
    var dense = labels.length > 40;

    var pnlCanvas = document.getElementById("pnlChart");
    if (pnlCanvas) {
        new Chart(pnlCanvas.getContext("2d"), {
            type: "line",
            data: {
                labels: labels,
                datasets: [{
                    label: "Cumulative P&L ($)",
                    data: cumPnl,
                    borderColor: "#10b981",
                    backgroundColor: "rgba(16, 185, 129, 0.12)",
                    borderWidth: 2,
                    pointRadius: dense ? 0 : 2.5,
                    pointHoverRadius: 4,
                    fill: true,
                    tension: 0.15
                }]
            },
            options: {
                responsive: true,
                maintainAspectRatio: false,
                interaction: { mode: "index", intersect: false },
                scales: {
                    x: {
                        grid: { color: "#1e293b" },
                        ticks: { color: "#94a3b8", maxTicksLimit: 8, maxRotation: 0 }
                    },
                    y: {
                        grid: { color: "#1e293b" },
                        ticks: {
                            color: "#94a3b8",
                            callback: function (v) { return "$" + Number(v).toLocaleString(); }
                        }
                    }
                },
                plugins: { legend: { display: false } }
            }
        });
    }

    var ddCanvas = document.getElementById("drawdownChart");
    if (ddCanvas) {
        new Chart(ddCanvas.getContext("2d"), {
            type: "line",
            data: {
                labels: labels,
                datasets: [{
                    label: "Drawdown (%)",
                    data: drawdown,
                    borderColor: "#ef4444",
                    backgroundColor: "rgba(239, 68, 68, 0.15)",
                    borderWidth: 2,
                    pointRadius: dense ? 0 : 2.5,
                    pointHoverRadius: 4,
                    fill: true,
                    tension: 0.1
                }]
            },
            options: {
                responsive: true,
                maintainAspectRatio: false,
                interaction: { mode: "index", intersect: false },
                scales: {
                    x: {
                        grid: { color: "#1e293b" },
                        ticks: { color: "#94a3b8", maxTicksLimit: 6, maxRotation: 0 }
                    },
                    y: {
                        grid: { color: "#1e293b" },
                        ticks: {
                            color: "#94a3b8",
                            callback: function (v) { return Number(v).toFixed(1) + "%"; }
                        },
                        reverse: true
                    }
                },
                plugins: { legend: { display: false } }
            }
        });
    }

    // Auto-refresh dashboard every 30s to reflect live cycles
    window.setTimeout(function () {
        window.location.reload();
    }, 30000);
})();
