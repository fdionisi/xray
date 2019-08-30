import { React, ReactDOM, App, buildViewRegistry } from "xray_ui";
import * as uuid from "uuid/v4";
import XrayClient from "./client";
const $ = React.createElement;

const UUID_KEY = `xray-replica-id`
const getReplicaId = () => {
  const storedId = localStorage.getItem(UUID_KEY)
  if (storedId) {
    return JSON.parse(storedId)
  }

  const id = new Array();
  uuid(null, id)
  localStorage.setItem(UUID_KEY, JSON.stringify(id))

  return id
}

const client = new XrayClient(new Worker("worker.js"));
const websocketURL = new URL("/ws", window.location.href);
websocketURL.protocol = "ws";
client.sendMessage({
  type: "ConnectToWebsocket",
  url: websocketURL.href,
  replica_id: getReplicaId()
});

const viewRegistry = buildViewRegistry(client);

let initialRender = true;
client.onMessage(message => {
  switch (message.type) {
    case "UpdateWindow":
      viewRegistry.update(message);
      if (initialRender) {
        ReactDOM.render(
          $(App, { inBrowser: true, viewRegistry }),
          document.getElementById("app")
        );
        initialRender = false;
      }
      break;
    default:
      console.warn("Received unexpected message", message);
  }
});
