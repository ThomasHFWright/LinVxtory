// SPDX-License-Identifier: MIT
#define _WIN32_WINNT 0x0a00
#include <windows.h>
#include <stdio.h>
#include <stdint.h>
#include <string.h>
#define N 22552
static void *views[N];
static uint64_t digest=1469598103934665603ull;
static unsigned results,failures;
static void note(unsigned value) { digest=(digest^value)*1099511628211ull; results++; }
static void checked(int okay,int line) { note(!!okay); if (!okay) {failures++; fprintf(stderr,"FAILED line=%d error=%lu\n",line,GetLastError());} }
#define require(x) checked(!!(x),__LINE__)
static unsigned rng=0x724937u;
static unsigned next(void) { rng=rng*1664525u+1013904223u; return rng; }
static void *base(unsigned i) {return (void *)(uintptr_t)(0x40000000000ull+(uint64_t)i*0x20000);}
int main(int argc,char **argv) {
 int child=argc>1; unsigned n=child?257:N; DWORD written; LARGE_INTEGER t0,t1,freq;
 HANDLE mapping=CreateFileMappingW(INVALID_HANDLE_VALUE,NULL,PAGE_READWRITE,0,0x20000,NULL);
 require(mapping!=NULL); if(!mapping)return 2;
 Sleep(child?0:2500); QueryPerformanceFrequency(&freq); QueryPerformanceCounter(&t0);
 for(unsigned i=0;i<n;i++) { views[i]=MapViewOfFileEx(mapping,FILE_MAP_ALL_ACCESS,0,0,0x10000,base(i)); require(views[i]==base(i)); }
 for(unsigned i=0;i<n;i+=97) if(views[i]) { *(volatile unsigned *)views[i]=i^0x55335533; require(*(volatile unsigned*)views[n-1]==(i^0x55335533)); }
 for(unsigned j=0;j<n*2;j++) {
  unsigned i=next()%n;
  if(j%4==0 && views[i]) {
   SetLastError(0); void *p=MapViewOfFileEx(mapping,FILE_MAP_ALL_ACCESS,0,0,0x20000,base(i));
   note(p==NULL); note(p?0:GetLastError()); if(p){failures++;UnmapViewOfFile(p);}
  } else if(views[i]) { require(UnmapViewOfFile(views[i])); views[i]=NULL; }
  else { views[i]=MapViewOfFileEx(mapping,FILE_MAP_ALL_ACCESS,0,0,0x10000,base(i)); require(views[i]==base(i)); }
 }
 for(unsigned i=0;i<n;i++) if(views[i]) require(UnmapViewOfFile(views[i]));
 /* Actual placeholder replacement API used by the runtime, without graphics. */
 void *placeholder=VirtualAlloc2(GetCurrentProcess(),NULL,0x10000,MEM_RESERVE|MEM_RESERVE_PLACEHOLDER,PAGE_NOACCESS,NULL,0);
 require(placeholder!=NULL);
 if(placeholder) {
  void *p=MapViewOfFile3(mapping,GetCurrentProcess(),placeholder,0,0x10000,MEM_REPLACE_PLACEHOLDER,PAGE_READWRITE,NULL,0);
  require(p==placeholder);
  if(p){ *(volatile unsigned *)p=0xdead1234; require(UnmapViewOfFile2(GetCurrentProcess(),p,MEM_PRESERVE_PLACEHOLDER));
   p=MapViewOfFile3(mapping,GetCurrentProcess(),placeholder,0,0x10000,MEM_REPLACE_PLACEHOLDER,PAGE_READWRITE,NULL,0);
   require(p==placeholder); if(p){ require(*(volatile unsigned *)p==0xdead1234); require(UnmapViewOfFile2(GetCurrentProcess(),p,MEM_PRESERVE_PLACEHOLDER)); }
  }
  require(VirtualFree(placeholder,0,MEM_RELEASE));
 }
 CloseHandle(mapping); QueryPerformanceCounter(&t1);
 printf("%s results=%u digest=%016llx failures=%u\n",child?"child":"parent",results,(unsigned long long)digest,failures); fflush(stdout);
 fprintf(stderr,"%s elapsed_ms=%.3f\n",child?"child":"parent",1000.0*(t1.QuadPart-t0.QuadPart)/freq.QuadPart);
 if(!child) {
  WCHAR exe[32768],command[32768]; STARTUPINFOW si={sizeof(si)}; PROCESS_INFORMATION pi;
  si.dwFlags=STARTF_USESTDHANDLES; si.hStdOutput=GetStdHandle(STD_OUTPUT_HANDLE); si.hStdError=GetStdHandle(STD_ERROR_HANDLE); si.hStdInput=GetStdHandle(STD_INPUT_HANDLE);
  SetHandleInformation(si.hStdOutput,HANDLE_FLAG_INHERIT,HANDLE_FLAG_INHERIT); SetHandleInformation(si.hStdError,HANDLE_FLAG_INHERIT,HANDLE_FLAG_INHERIT);
  GetModuleFileNameW(NULL,exe,32768); swprintf(command,32768,L"\"%ls\" --child",exe);
  if(!CreateProcessW(exe,command,NULL,NULL,TRUE,CREATE_NO_WINDOW,NULL,NULL,&si,&pi)) { fprintf(stderr,"CreateProcess error=%lu cmd=%ls\n",GetLastError(),command); return 3; }
  if(WaitForSingleObject(pi.hProcess,30000)!=WAIT_OBJECT_0) return 4;
  DWORD status; GetExitCodeProcess(pi.hProcess,&status); CloseHandle(pi.hThread);CloseHandle(pi.hProcess);
  printf("child_exit=%lu\n",status); if(status) return 5;
 }
 return failures?1:0;
}
