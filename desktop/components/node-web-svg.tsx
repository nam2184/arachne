type NodeWebSvgProps = {
  nodeTone?: "black" | "white";
  className?: string;
};

const nodes = [
  { id: "a", x: 18, y: 24, r: 11 },
  { id: "b", x: 56, y: 12, r: 9 },
  { id: "c", x: 108, y: 28, r: 11 },
  { id: "d", x: 34, y: 70, r: 12 },
  { id: "e", x: 70, y: 62, r: 14 },
  { id: "f", x: 116, y: 86, r: 10 },
  { id: "g", x: 12, y: 112, r: 9 },
  { id: "h", x: 58, y: 116, r: 11 },
];

const edges = [
  ["a", "b"],
  ["b", "c"],
  ["a", "d"],
  ["d", "g"],
  ["b", "e"],
  ["e", "c"],
  ["d", "e"],
  ["e", "f"],
  ["g", "h"],
  ["h", "f"],
  ["e", "h"],
  ["c", "f"],
] as const;

const nodeById = new Map(nodes.map((node) => [node.id, node]));

function edgePath(fromId: string, toId: string) {
  const from = nodeById.get(fromId);
  const to = nodeById.get(toId);

  if (!from || !to) return "";

  return `M${from.x} ${from.y} L${to.x} ${to.y}`;
}

export function NodeWebSvg({ nodeTone = "white", className }: NodeWebSvgProps) {
  const whiteNodes = nodeTone === "white";
  const nodeFill = whiteNodes ? "white" : "black";
  const nodeStroke = whiteNodes ? "black" : "white";
  const lineStroke = whiteNodes ? "rgba(255,255,255,0.74)" : "rgba(0,0,0,0.74)";

  return (
    <svg className={className} viewBox="0 0 128 128" fill="none" role="img" aria-label="Arachne node web logo">
      <rect width="128" height="128" fill="transparent" />
      <g stroke={lineStroke} strokeLinecap="round" strokeLinejoin="round" strokeWidth="5.5">
        {edges.map(([from, to]) => (
          <path key={`${from}-${to}`} d={edgePath(from, to)} />
        ))}
      </g>
      <g fill={nodeFill} stroke={nodeStroke} strokeWidth="4">
        {nodes.map((node) => (
          <circle key={node.id} cx={node.x} cy={node.y} r={node.r} />
        ))}
      </g>
    </svg>
  );
}
