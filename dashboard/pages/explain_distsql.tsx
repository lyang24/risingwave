import { Box, Button, Stack, Textarea } from "@chakra-ui/react";
import { Fragment } from "react";
import Title from "../components/Title";
import React, { useState } from "react";
import ReactFlow, { Background, Controls, MiniMap, Node, Edge } from "react-flow-renderer";
import styled from "styled-components";
import NodeType from "./node";
import { Graphviz } from "graphviz-react";
import * as d3 from "d3"
import { parse } from "graphlib-dot";

const ContainerDiv = styled(Box)`
  font-family: sans-serif;
  text-align: left;
`;

const DemoArea = styled(Box)`
  width: 100%;
  height: 80vh;
`;

const position = {
  x: 200,
  y: 100,
};

const nodeTypes = { node: NodeType };

function getColor() {
  return (
    "hsl(" +
    360 * Math.random() +
    "," +
    (25 + 70 * Math.random()) +
    "%," +
    (85 + 10 * Math.random()) +
    "%)"
  );
}

function getStyle() {
  return {
    background: `linear-gradient(${getColor()}, white, white)`,
    height: 50,
    width: 150,
    border: "0.5px solid black",
    padding: "5px",
    "border-radius": "5px",
  };
}

function layoutElements(nodeList, edgeList, stageToNode, rootStageId) {
  const idToNode = new Map();
  nodeList.forEach((node) => {
    idToNode.set(node.id, [{ id: node.id, children: [] }, node]);
  });

  edgeList.forEach((edge) => {
    const sourceNode = idToNode.get(edge.source)[0];
    const targetNode = idToNode.get(edge.target)[0];
    sourceNode.children.push(targetNode);
  });

  var rootNode = idToNode.get(stageToNode[rootStageId].toString())[0];
  var root = d3.hierarchy(rootNode);
  var tree = d3.tree().nodeSize([60, 180]);
  const treeRoot = tree(root);

  treeRoot.each((treeNode) => {
    const node = idToNode.get(treeNode.data.id)[1];
    if (node == undefined) return;
    node.position = {
      x: treeNode.y,
      y: treeNode.x,
    };
  });
}

function parseSubElements(
  root: any,
  stage: string,
  style: any,
  nodeList: any,
  edgeList: any,
  visited: Set<string>,
  nodeStagePairs: number[][]
) {
  if (root.children.length == 0) return
  for (var i = 0; i < root.children.length; i++) {
    const child = root.children[i]
    var edge = {
      id: `e${root.plan_node_id}-${child.plan_node_id}`,
      source: root.plan_node_id.toString(),
      target: child.plan_node_id.toString(),
      type: "smoothstep",
    }
    edgeList.push(edge)
    if (visited.has(child.plan_node_id)) continue
    var node = {
      id: child.plan_node_id.toString(),
      data: {
        label: `#${child.plan_node_id} ${child.plan_node_type}`,
        stage: stage,
        content: Object.values(child.schema),
      },
      position: position,
      type: "node",
      style: style,
    }
    if (child.source_stage_id != null) {
      nodeStagePairs.push([child.plan_node_id, child.source_stage_id])
    }
    parseSubElements(
      child,
      stage,
      style,
      nodeList,
      edgeList,
      visited,
      nodeStagePairs
    )
    nodeList.push(node)
  }
}


function parseElements(input: any) {
  var nodeList: Node[] = []
  var edgeList: Edge[] = []
  var stages: { [key: number]: Stage } = input.stages
  var visited: Set<string> = new Set()
  var stageToNode: { [key: string]: number } = {}
  var nodeStagePairs: number[][] = []

  const rootStageId = input.root_stage_id.toString()
  for (const [key, value] of Object.entries(stages)) {
    const root: PlanNode = value.root
    stageToNode[key] = root.plan_node_id
    var style = getStyle()
    var node = {
      id: root.plan_node_id.toString(),
      data: {
        label: `#${root.plan_node_id} ${root.plan_node_type}`,
        stage: key,
        content: Object.values(root.schema),
      },
      position: position,
      type: "node",
      style: style,
    }
    if (root.source_stage_id != null) {
      nodeStagePairs.push([root.plan_node_id, root.source_stage_id])
    }
    visited.add(node.id)
    parseSubElements(
      root,
      key,
      style,
      nodeList,
      edgeList,
      visited,
      nodeStagePairs
    )
    nodeList.push(node)
  }
  for (var i = 0; i < nodeStagePairs.length; i++) {
    var source = nodeStagePairs[i][0]
    var target = stageToNode[nodeStagePairs[i][1].toString()]
    var edge = {
      id: `e${target}-${source}`,
      source: source.toString(),
      target: target.toString(),
      type: "smoothstep",
    }
    edgeList.push(edge)
  }

  layoutElements(nodeList, edgeList, stageToNode, rootStageId)
  return { node: nodeList, edge: edgeList }
}

export default function Explain() {
  const [input, setInput] = useState(""); // Input state for DOT or JSON
  const [isUpdate, setIsUpdate] = useState(false); // Flag to track changes in input
  const [nodes, setNodes] = useState([]); // ReactFlow nodes (JSON)
  const [edges, setEdges] = useState([]); // ReactFlow edges (JSON)
  const [dotInput, setDotInput] = useState(""); // DOT input for Graphviz
  const [isDotParsed, setIsDotParsed] = useState(false); // Flag for DOT parsing

  const handleChange = (event) => {
    setInput(event.target.value);
    setIsUpdate(true);
  };

  const handleParseJson = () => {
    if (!isUpdate) return
    try {
      const jsonInput = JSON.parse(input)
      var elements = parseElements(jsonInput)
      setEdges(elements.edge)
      setNodes(elements.node)
      setIsUpdate(false)
      setIsDotParsed(false);
    } catch (e) {
      console.error(e);
    }
  };

  const handleParseDot = () => {
    if (!isUpdate) return
    try {
      // Validate
      parse(input);
      // Attempt to set DOT input for Graphviz rendering
      setDotInput(input);
      setIsDotParsed(true); // Set flag to render Graphviz
    } catch (error) {
      alert("Invalid DOT input! Please provide valid DOT syntax."); // Display an error alert
      console.error("DOT parsing error:", error); // Log the error for debugging
    }
  };

  return (
    <Fragment>
      <Box p={3}>
        <Title>Distributed Plan Explain</Title>
        <Stack direction="row" spacing={4} align="center">
          <Textarea
            name="input graph"
            placeholder="Input DOT or JSON"
            value={input}
            onChange={handleChange}
            style={{ width: "1000px", height: "100px" }}
          />
          <Button
            colorScheme="blue"
            onClick={handleParseJson}
            style={{ width: "80px", height: "100px" }}
          >
            Parse JSON
          </Button>
          <Button
            colorScheme="green"
            onClick={handleParseDot}
            style={{ width: "80px", height: "100px" }}
          >
            Parse DOT
          </Button>
        </Stack>

        <ContainerDiv fluid>
          <DemoArea>
            {/* Render ReactFlow if nodes and edges exist */}
            {nodes.length > 0 && edges.length > 0 && !isDotParsed && (
              <ReactFlow nodes={nodes} edges={edges} nodeTypes={nodeTypes}>
                <MiniMap />
                <Controls />
                <Background color="#aaa" gap={16} />
              </ReactFlow>
            )}

            {/* Render Graphviz visualization only when DOT input is provided */}
            {isDotParsed && dotInput && (
              <Box mt={4}>
                <Graphviz
                  dot={dotInput} // Pass the DOT input for visualization
                  options={{ width: 600, height: 400 }}
                />
              </Box>
            )}
          </DemoArea>
        </ContainerDiv>
      </Box>
    </Fragment>
  );
}
