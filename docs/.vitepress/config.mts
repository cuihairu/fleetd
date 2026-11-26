import { defineConfig } from "vitepress";

const sidebar = [
  {
    text: "入门",
    items: [
      { text: "舰队术语表", link: "/vocabulary" },
      { text: "快速上手", link: "/getting-started" }
    ]
  },
  {
    text: "设计文档",
    items: [
      { text: "Worker 节点模型", link: "/worker-design" },
      { text: "Relay 心跳中继", link: "/relay-design" },
      { text: "Vault 秘密分发", link: "/vault-design" },
      { text: "Seat 控制", link: "/seat-control" },
      { text: "Orca 模式", link: "/orca-patterns" }
    ]
  }
];

export default defineConfig({
  lang: "zh-CN",
  title: "fleetd",
  description: "跨机舰队控制守护 —— flagship / ship / admiral",
  themeConfig: {
    nav: [{ text: "设计文档", link: "/worker-design" }],
    sidebar,
    socialLinks: [],
    outline: { level: [2, 3], label: "本页目录" },
    docFooter: { prev: "上一页", next: "下一页" },
    returnToTopLabel: "回到顶部"
  }
});
