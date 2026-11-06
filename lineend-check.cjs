const fs=require("fs");
const b=fs.readFileSync("C:/Users/aiden/Projects/SaveSync/savesync-desktop/mockups/home-menu-preview.html");
const crlf=(b.toString("latin1").match(/\r\n/g)||[]).length;
const lf=(b.toString("latin1").match(/\n/g)||[]).length;
console.log("bytes:",b.length,"LF:",lf,"CRLF:",crlf,"lone LF:",lf-crlf);
console.log("ends with newline:", b.slice(-1).toString()==="\n");
