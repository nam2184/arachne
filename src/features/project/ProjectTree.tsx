import { useProjectStore } from "./projectStore";

export function TechStackBadges({ stack }: { stack: string[] }) {
  return (
    <div className="tech-stack-badges">
      {stack.map((tech) => (
        <span key={tech} className="badge">{tech}</span>
      ))}
    </div>
  );
}

export function ProjectTree() {
  return <div className="project-tree">Project tree placeholder</div>;
}